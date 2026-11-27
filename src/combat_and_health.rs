use megu_core::projectile::{
    step_projectile_kinematics, sweep_projectile_segment, Bounds as ProjectileBounds,
    Contact as ProjectileContact, FlightContacts, Impact as ProjectileImpact, ProjectileKind,
};

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
    /// Remaining ticks the bite dip is shown — vanilla's submerge animation.
    dip: i32,
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

/// Poison-tolerant guards: a panic while a store is held must never take the
/// tick down. These are the file's only lock spellings.
fn lk<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
fn rd<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}
fn wr<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
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
/// The `SimXp` view lets the `megu_core::zero_change_sim` merge kernel run
/// the live orb pool — same cell table, chain order and merge contract as
/// the hand-rolled scan it replaced.
impl megu_core::zero_change_sim::SimXp for Xp {
    fn eid(&self) -> i32 {
        self.eid
    }
    fn pos(&self) -> [f64; 3] {
        [self.x, self.y, self.z]
    }
    fn age(&self) -> i32 {
        self.age
    }
    fn value(&self) -> i16 {
        self.val
    }
    fn set_value(&mut self, val: i16) {
        self.val = val;
    }
}
static XPS: OnceLock<Mutex<Vec<Xp>>> = OnceLock::new();
fn xps() -> &'static Mutex<Vec<Xp>> {
    XPS.get_or_init(|| Mutex::new(Vec::new()))
}

// ---------- entity dimension side table ----------
// `Arrow`, `Primed`, `Bobber`, `Pearl`, `Snowball`, `Firework` and `Xp` are
// built with struct literals in files this lane does not own, so the dimension
// cannot join the struct: it lives here, keyed by entity id. `entity_dim_of`
// answers `DimId::Overworld` for an eid nobody registered — the dimension those
// callers always assumed. `clear_entity_dim` retires the entry when the entity
// despawns so the map cannot grow one row per entity ever spawned.
static ENTITY_DIMS: OnceLock<RwLock<HashMap<i32, DimId>>> = OnceLock::new();
fn entity_dims() -> &'static RwLock<HashMap<i32, DimId>> {
    ENTITY_DIMS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// The dimension `eid` was registered under, `DimId::Overworld` when none was.
pub(crate) fn entity_dim_of(eid: i32) -> DimId {
    rd(entity_dims())
        .get(&eid)
        .copied()
        .unwrap_or(DimId::Overworld)
}

/// Record the dimension an entity lives in. `DimId::Overworld` removes the
/// entry instead — it is already the default answer.
pub(crate) fn set_entity_dim(eid: i32, dim: DimId) {
    let mut dims = wr(entity_dims());
    if dim == DimId::Overworld {
        dims.remove(&eid);
    } else {
        dims.insert(eid, dim);
    }
}

/// Forget an entity's dimension on despawn: eids are never reused, but the
/// entry would otherwise sit in the table until process end.
pub(crate) fn clear_entity_dim(eid: i32) {
    wr(entity_dims())
        .remove(&eid);
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

/// Spawn an experience orb in `dim`, marking it merge-pending so the next
/// `drop_tick` pass can collapse a spawn burst inside the same tick. The
/// orb's dimension is the side-table entry because `Xp` carries no `dim`
/// field (it is built with struct literals outside this lane).
fn xp_spawn_dim(dim: DimId, x: f64, y: f64, z: f64, val: i16) -> i32 {
    let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
    lk(xps()).push(Xp {
        eid,
        x,
        y,
        z,
        val,
        age: 0,
    });
    set_entity_dim(eid, dim);
    lk(xp_pending())
        .insert(eid);
    eid
}

/// `xp_spawn` for callers that predate per-entity dimensions: the orb lands in
/// the Overworld, the dimension they always assumed.
fn xp_spawn(x: f64, y: f64, z: f64, val: i16) -> i32 {
    xp_spawn_dim(DimId::Overworld, x, y, z, val)
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
    // The keeper/donor index staging rides `sim_graph`'s retained tick scratch
    // instead of a fresh `Vec` every merge pass.
    let mut pool = XP_POOL_SCRATCH.with(|s| std::mem::take(&mut s.borrow_mut().indices));
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
        XP_POOL_SCRATCH.with(|s| {
            pool.clear();
            s.borrow_mut().indices = pool;
        });
        return Vec::new();
    }
    // Merge only within a dimension: two orbs at the same coordinates in
    // different dimensions are 1.0 apart in no world, so each dimension's
    // slice of the pool runs the kernel on its own.
    let mut groups: Vec<(DimId, Vec<usize>)> = Vec::new();
    for &i in &pool {
        let dim = entity_dim_of(xs[i].eid);
        if let Some((_, group)) = groups.iter_mut().find(|(d, _)| *d == dim) {
            group.push(i);
        } else {
            groups.push((dim, vec![i]));
        }
    }
    let mut eaten = HashSet::new();
    let mut order = Vec::new();
    let mut spill = Vec::new();
    for (dim, gpool) in groups {
        let mut vals: Vec<u64> = gpool.iter().map(|&i| xs[i].val as u64).collect();
        xp_merge_run(&gpool, xs, &mut vals);
        for (k, &i) in gpool.iter().enumerate() {
            if vals[k] == 0 {
                if eaten.insert(xs[i].eid) {
                    order.push(xs[i].eid);
                }
            } else {
                let keep = vals[k].min(XP_ORB_MAX_VAL);
                let excess = vals[k] - keep;
                xs[i].val = keep as i16;
                if excess > 0 {
                    spill.push((xs[i].x, xs[i].y, xs[i].z, excess, dim));
                }
            }
        }
    }
    if !eaten.is_empty() {
        xs.retain(|x| !eaten.contains(&x.eid));
        for &eid in &eaten {
            clear_entity_dim(eid);
        }
    }
    for (x, y, z, mut left, dim) in spill {
        while left > 0 {
            let take = left.min(XP_ORB_MAX_VAL) as i16;
            let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
            xs.push(Xp {
                eid,
                x,
                y,
                z,
                val: take,
                age: 0,
            });
            set_entity_dim(eid, dim);
            left -= u64::from(take as u16);
        }
    }
    XP_POOL_SCRATCH.with(|s| {
        pool.clear();
        s.borrow_mut().indices = pool;
    });
    order
}

thread_local! {
    /// `xp_merge_pending_locked`'s keeper/donor index list: one retained
    /// `sim_graph` tick scratch instead of a `Vec` allocated per merge pass.
    static XP_POOL_SCRATCH: std::cell::RefCell<megu_core::sim_graph::TickScratch> =
        const {
            std::cell::RefCell::new(megu_core::sim_graph::TickScratch {
                indices: Vec::new(),
            })
        };
}

/// Collapse freshly spawned orbs into their older neighbours — the merge the
/// env pass would do, done the same tick the storm lands so a farm's 2,000
/// orbs reach the pickup scan as a handful. Runs only once `xps()` outgrows
/// `XP_MERGE_FLOOR`: below it the env merge is already cheap, and the pinned
/// save-payload fixture — which never routes a spawn through `xp_spawn` —
/// cannot observe this path at all.
///
/// The `xps` store mixes dimensions now, so `xp_merge_pending_locked`
/// partitions the pool by `entity_dim_of` first — the 1.0-block radius is the
/// only spatial gate inside a same-dimension group.
fn xp_merge_pending() {
    let pending: HashSet<i32> = {
        let mut p = lk(xp_pending());
        if p.is_empty() {
            return;
        }
        std::mem::take(&mut *p)
    };
    let mut xs = lk(xps());
    if xs.len() <= XP_MERGE_FLOOR {
        return;
    }
    let eaten = xp_merge_pending_locked(&mut xs, &pending);
    drop(xs);
    for eid in eaten {
        broadcast_seen_destroy(eid);
    }
}

// Vanilla 1.21.1 attack cooldown: seconds per full charge. The item
// `attribute_modifiers` encode attack-speed deltas from bare-handed 4.0.
fn attack_cooldown(item: i64) -> f64 {
    if item_to_name(item) == Some("mace") {
        return 1.0 / 0.6;
    }
    if item_to_name(item) == Some("trident") {
        return 1.0 / 1.1;
    }
    if !(818..=847).contains(&item) {
        return 0.25;
    }
    let band = (item - 818) / 5;
    let speed = match (item - 818) % 5 {
        0 => 1.6, // swords
        1 => 1.0, // shovels
        2 => 1.2, // pickaxes
        3 => match band {
            0 | 1 => 0.8,
            2 => 1.0,
            3 => 0.9,
            _ => 1.0,
        },
        _ => match band {
            0 | 2 => 1.0,
            1 => 2.0,
            3 => 3.0,
            _ => 4.0,
        },
    };
    1.0 / speed
}

/// `data/minecraft/enchantment/sharpness.json`: linear `1.0 + 0.5*(level-1)`.
fn vanilla_sharpness_damage(level: i64) -> f32 {
    if level > 0 { 0.5 * level as f32 + 0.5 } else { 0.0 }
}

/// Total vanilla melee attack damage of the held item, including the player's
/// bare-handed base point. Tool rows are wood, stone, gold, iron, diamond,
/// netherite in the numeric registry's order.
fn vanilla_weapon_attack_damage(item: i64) -> f32 {
    if item_to_name(item) == Some("mace") {
        return 6.0;
    }
    if item_to_name(item) == Some("trident") {
        return 9.0;
    }
    let band = if (818..=847).contains(&item) { (item - 818) / 5 } else { -1 };
    let kind = if (818..=847).contains(&item) { (item - 818) % 5 } else { -1 };
    let table = match kind {
        0 => [4.0, 5.0, 4.0, 6.0, 7.0, 8.0],
        1 => [2.5, 3.5, 2.5, 4.5, 5.5, 6.5],
        2 => [2.0, 3.0, 2.0, 4.0, 5.0, 6.0],
        3 => [7.0, 9.0, 7.0, 9.0, 9.0, 10.0],
        4 => [1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
        _ => [1.0; 6],
    };
    table[band.clamp(0, 5) as usize]
}

fn vanilla_tool_kind(item: i64) -> i64 {
    if (818..=847).contains(&item) { (item - 818) % 5 } else { -1 }
}

// ---------- attribute modifiers on equipped items ----------
// Protocol 767 attribute registry ids (`minecraft:attribute`); the same order
// `attr_key_id` encodes on the wire.
const ATTR_ARMOR: i32 = 0;
const ATTR_ARMOR_TOUGHNESS: i32 = 1;
const ATTR_ATTACK_DAMAGE: i32 = 2;
const ATTR_ATTACK_SPEED: i32 = 4;
const ATTR_EXPLOSION_KNOCKBACK_RESISTANCE: i32 = 8;
const ATTR_KNOCKBACK_RESISTANCE: i32 = 15;
const ATTR_MAX_HEALTH: i32 = 18;

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
    let inv = lk(&c.inv);
    let held_slot = *lk(&c.held);
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
        ATTR_EXPLOSION_KNOCKBACK_RESISTANCE,
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
    lk(equip_attrs()).insert(c.name.clone(), m);
}

/// Vanilla `Player.currentItemAttackStrengthDelay` source: the effective
/// `generic.attack_speed`. A stack that carries an `attribute_modifiers`
/// component replaces the item's vanilla default (vanilla defaults are
/// components), so the whole attribute is resolved through the codec; a stack
/// without one keeps the table speed — the table encodes `4.0 + item delta`,
/// so the delta is preserved on top of any `/attribute`-set base.
fn melee_attack_speed(me: &Client, item: i64) -> f64 {
    let base = lk(attrs())
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
// vanilla 1.9+ `Player.getAttackStrengthScale(0.5)`: ticks since the last
// attack reset scaled by `20 / attack_speed`, clamped to 1.0. Driven by
// TICK_IDX so `/tick freeze` and tick lag freeze the recharge instead of
// drifting on wall-clock time.
fn melee_charge(me: &Arc<Client>, item: i64) -> f32 {
    let speed_eff = lk(&me.effs).iter().fold(1.0_f32, |speed, effect| {
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
    let held_slot = *lk(&me.held);
    let mut lm = lk(&me.last_melee);
    let wall_ticks =
        u64::try_from(lm.elapsed().as_millis() / u128::from(TICK_MS)).unwrap_or(u64::MAX);
    let mut map = lk(attack_tickers());
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
/// `#minecraft:undead` (1.21.1): every member Smite's +2.5/level bonus applies to.
const ENCHANT_TARGET_UNDEAD: &[&str] = &[
    "bogged",
    "drowned",
    "husk",
    "phantom",
    "skeleton",
    "skeleton_horse",
    "stray",
    "wither",
    "wither_skeleton",
    "zombie",
    "zombie_horse",
    "zombie_villager",
    "zombified_piglin",
    "zoglin",
];
/// `#minecraft:arthropod` (1.21.1): Bane of Arthropods' damage/slowdown family.
const ENCHANT_TARGET_ARTHROPOD: &[&str] =
    &["bee", "cave_spider", "endermite", "silverfish", "spider"];
/// `#minecraft:sensitive_to_impaling` (1.21.1) — the whole aquatic family. The
/// tag name differs from the issue's `#minecraft:aquatic` shorthand; the
/// membership is the same 12 rows vanilla lists.
const ENCHANT_TARGET_AQUATIC: &[&str] = &[
    "axolotl",
    "cod",
    "dolphin",
    "elder_guardian",
    "glow_squid",
    "guardian",
    "pufferfish",
    "salmon",
    "squid",
    "tadpole",
    "tropical_fish",
    "turtle",
];
fn enchant_target_in(etype: i64, names: &[&str]) -> bool {
    entity_spec_by_id(etype).is_some_and(|entity| {
        names.contains(&entity.name.strip_prefix("minecraft:").unwrap_or(""))
    })
}
fn enchantment_targets_undead(etype: i64) -> bool {
    enchant_target_in(etype, ENCHANT_TARGET_UNDEAD)
}
fn enchantment_targets_arthropod(etype: i64) -> bool {
    enchant_target_in(etype, ENCHANT_TARGET_ARTHROPOD)
}
fn enchantment_targets_aquatic(etype: i64) -> bool {
    enchant_target_in(etype, ENCHANT_TARGET_AQUATIC)
}
/// The three type-gated enchantments that pay flat damage: Smite (34) against
/// `#minecraft:undead`, Bane of Arthropods (1) against `#minecraft:arthropod`
/// and Impaling (15) against `#minecraft:sensitive_to_impaling`.
const ENCHANT_SMITE: i64 = 34;
const ENCHANT_BANE_OF_ARTHROPODS: i64 = 1;
const ENCHANT_IMPALING: i64 = 15;
/// One enchantment's `minecraft:damage` effect value: `2.5` per level
/// (`data/minecraft/enchantment/{smite,bane_of_arthropods,impaling}.json`, the
/// `add`/`linear` line every one of them carries).
const TYPED_ENCHANT_DAMAGE_PER_LEVEL: f32 = 2.5;

/// `EnchantmentHelper.modifyDamage`'s type-gated terms (1.21.1): every matching
/// enchantment on `comps` adds a flat `2.5 * level` to the hit. `Player.attack`
/// and `ThrownTrident.onHitEntity` both run this, so melee and thrown tridents
/// share the one formula.
fn typed_damage_bonus(comps: &Comps, etype: i64) -> f32 {
    [
        (ENCHANT_SMITE, enchantment_targets_undead(etype)),
        (ENCHANT_BANE_OF_ARTHROPODS, enchantment_targets_arthropod(etype)),
        (ENCHANT_IMPALING, enchantment_targets_aquatic(etype)),
    ]
    .into_iter()
    .filter(|&(_, applies)| applies)
    .map(|(enchant, _)| TYPED_ENCHANT_DAMAGE_PER_LEVEL * enchant_level(comps, enchant) as f32)
    .sum()
}
/// Bane of Arthropods' post-hit rider (vanilla `DamageEnchantment.doPostAttack`):
/// Slowness IV (amplifier 3) for a random `20 + nextInt(10 * level)` ticks,
/// i.e. the inclusive window `20..=(20 + 10 * level)`.
#[cfg(test)]
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
#[cfg(test)]
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
fn melee_hit_damage(base: f32, enchant_bonus: f32, charge: f32, critical: bool) -> f32 {
    melee_hit_damage_with_bonus(base, enchant_bonus, charge, critical, 0.0)
}

/// Exact `Player.attack` ordering: weapon/effect base is charge-scaled, the
/// mace smash bonus is added after that scale, the critical factor multiplies
/// only this base-plus-smash total, and enchantment damage (scaled by bare
/// charge) is added last.
fn melee_hit_damage_with_bonus(
    base: f32,
    enchant_bonus: f32,
    charge: f32,
    critical: bool,
    post_scale_bonus: f32,
) -> f32 {
    let scaled_base = base * (0.2 + 0.8 * charge * charge) + post_scale_bonus;
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
    lk(mob_fire_ticks()).insert(eid, ticks);
}
/// Vanilla `Player.attack`'s `target.igniteForSeconds(4 * fireAspectLevel)`:
/// Fire Aspect I burns 80 game ticks (4s), Fire Aspect II 160 (8s).
#[cfg(test)]
fn ignite_mob_fire_aspect(eid: i32, level: i64) {
    if level > 0 {
        ignite_mob(eid, fire_aspect_duration_ticks(level).max(0) as u32);
    }
}
/// The mob's remaining weapon/environmental burn, in server ticks.
#[cfg(test)]
fn mob_fire_ticks_of(eid: i32) -> u32 {
    lk(mob_fire_ticks())
        .get(&eid)
        .copied()
        .unwrap_or(0)
}
/// Vanilla water/rain extinguish (`Entity.isInWaterOrRain`): clears the burn.
fn extinguish_mob_fire(eid: i32) {
    lk(mob_fire_ticks()).remove(&eid);
}
/// One server tick of burning on the map store: decrements cleanly, pays
/// `1.0` HP every `MOB_BURN_TICK_GT` game ticks, never damages a fire-immune
/// mob, and goes out in water or rain. Returns the fire damage for the tick.
/// The per-tick driver is `mob_fire_and_persistence_tick_step`.
pub(crate) fn tick_mob_fire(eid: i32, tick: u64, fire_immune: bool, wet: bool) -> bool {
    let mut map = lk(mob_fire_ticks());
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
#[cfg(test)]
const SWEEP_KNOCKBACK: f64 = 0.4;
/// The vanilla gate around `sweepingAttack`: any sword (Sweeping Edge not
/// required), `attackStrengthScale > 0.9`, no crit, not sprinting, on ground.
/// The horizontal-speed cap (`walkDist - walkDistO < getSpeed()`) is checked
/// separately by `sweeping_movement_ok` since it needs live movement data.
fn sweeping_attack_gate(charge: f32, on_ground: bool, sprinting: bool, critical: bool) -> bool {
    charge > 0.9 && on_ground && !sprinting && !critical
}
/// Vanilla `Player.attack`: a sweep is refused while the attacker is moving
/// faster than their own movement speed (e.g. sprint-jumping through the hit).
#[cfg(test)]
fn sweeping_movement_ok(horizontal_speed: f64, speed_cap: f64) -> bool {
    horizontal_speed < speed_cap
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
    let slots = [45i16, 36 + *lk(&me.held)];
    let mut inv = lk(&me.inv);
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
enum WindChargeKind {
    Player,
    Breeze,
}

/// The `Level.explode` arguments each charge supplies, plus the direct-hit
/// damage its `onHitEntity` pays (the burst itself always deals 0).
#[derive(Clone, Copy, Debug)]
struct WindChargeProfile {
    /// Explosion power; the launch reach is `radius * 2` (vanilla `hurtEntities`
    /// scans `power * 2`).
    radius: f64,
    /// `AdvancedExplosionDamageCalculator.getKnockbackMultiplier`.
    knockback_multiplier: f64,
    /// `onHitEntity` damage.
    #[cfg(all(test, megu_vanilla_data))]
    direct_hit_damage: f32,
}

fn wind_charge_profile(kind: WindChargeKind) -> WindChargeProfile {
    match kind {
        // `coh` (`WindCharge`): explode power 1.2, `Optional.of(1.22f)` in the
        // `ddn` damage calculator, `ExplosionInteraction.TRIGGER`.
        WindChargeKind::Player => WindChargeProfile {
            radius: 1.2,
            knockback_multiplier: 1.22,
            #[cfg(all(test, megu_vanilla_data))]
            direct_hit_damage: 1.0,
        },
        // `cog` (`BreezeWindCharge`): explode power 3.0, same TRIGGER
        // interaction and an unmodified 1.0 knockback multiplier in 1.21.1.
        WindChargeKind::Breeze => WindChargeProfile {
            radius: 3.0,
            knockback_multiplier: 1.0,
            #[cfg(all(test, megu_vanilla_data))]
            direct_hit_damage: 1.0,
        },
    }
}

/// `Explosion.hurtEntities`' knockback arm: `(1 - dist/(power*2))` falloff,
/// `seenPercent`, the charge's `knockbackMultiplier` and the target's
/// knockback resistance, applied along the normalized direction to the
/// target's eye as an additive delta velocity. Vanilla skips entities at the
/// exact burst center; non-finite inputs scrub to `None`.
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
fn wind_charge_reset_fall(c: &Client, burst_y: f64) {
    if burst_y.is_finite() {
        *lk(&c.fall_from) = burst_y;
    }
}

/// `ExplosionInteraction.TRIGGER` on one block: non-iron doors, trapdoors and
/// fence gates flip `open`; levers flip `powered`; buttons press (the redstone
/// pass schedules the vanilla release); bells ring. Blocks held by a redstone
/// signal and the iron variants never move. Returns whether it triggered.
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
    if is_lever_name(name) {
        return set(st, "powered", if powered { "false" } else { "true" }, x, y, z);
    }
    if reg.in_tag(st, "minecraft:buttons") || name.ends_with("_button") {
        return !powered && set(st, "powered", "true", x, y, z);
    }
    if is_bell_name(name) {
        return !powered && set(st, "powered", "true", x, y, z);
    }
    let iron_path = block_path(name);
    let iron = iron_path == "iron_door" || iron_path == "iron_trapdoor";
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
        for mob in lk(mobs()).iter() {
            if Some(mob.etype) != name_to_etype("allay") {
                continue;
            }
            let (mx, my, mz, ..) = *lk(&mob.pos);
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
fn wind_charge_burst(dim: DimId, kind: WindChargeKind, impact: [f64; 3], thrower_eid: i32) {
    let profile = wind_charge_profile(kind);
    for c in client_snapshot() {
        if *lk(&c.dim) != dim
            || *lk(&c.gm) == 3
        {
            continue;
        }
        let (x, y, z, ..) = {
            let p = lk(&c.pos);
            *p
        };
        let eye = y
            + f64::from(player_eye_height(
                *lk(&c.sneaking),
                *lk(&c.fall_fly),
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
    for m in lk(mobs()).iter() {
        if m.dim != dim {
            continue;
        }
        let (mx, my, mz, ..) = *lk(&m.pos);
        let eye = my + f64::from(entity_eye_height_1211(m.etype));
        if let Some(dv) =
            evaluate_wind_charge_burst(kind, impact, [mx, my, mz], eye, 1.0, 0.0)
        {
            let mut p = lk(&m.pos);
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

// ---------- mace smash (Density damage / Wind Burst launch) ----------

/// Effects from an accepted mace hit, applied by the combat tick.
struct MaceSmash {
    dim: DimId,
    target: i32,
    center: [f64; 3],
    origin: [f64; 3],
    level: i64,
    fall: f32,
}
static MACE_SMASHES: OnceLock<Mutex<HashMap<i32, MaceSmash>>> = OnceLock::new();
fn mace_smashes() -> &'static Mutex<HashMap<i32, MaceSmash>> {
    MACE_SMASHES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Vanilla `WindBurstEnchantment` (id 41): the wielder is flung `0.3 +
/// 0.9 * level` blocks upward — I=1.2, II=2.1, III=3.0.
/// Vanilla `MaceItem.getAttackDamageBonus` tier arithmetic. The 1.5-block
/// airborne check is a gate, not an offset subtracted from the fall distance:
/// falls through 3 blocks pay 4 per block, the next 5 pay 2 per block, and
/// every block after 8 pays 1.
pub fn mace_smash_bonus_damage(fall_blocks: f32) -> f32 {
    if fall_blocks <= 0.0 {
        0.0
    } else if fall_blocks <= 3.0 {
        4.0 * fall_blocks
    } else if fall_blocks <= 8.0 {
        12.0 + 2.0 * (fall_blocks - 3.0)
    } else {
        fall_blocks + 14.0
    }
}

/// Combined unenchanted smash and Density bonus for one qualifying hit.
pub fn mace_smash_damage_bonus(fall_blocks: f32, density_level: i32) -> f32 {
    if fall_blocks <= 1.5 {
        0.0
    } else {
        mace_smash_bonus_damage(fall_blocks)
            + density_smash_bonus_damage(density_level.max(0), fall_blocks)
    }
}

/// Breach reduces the victim's armor effectiveness by 15% per level before
/// protection enchantments. `armor_dmg_factor` returns the fraction of damage
/// surviving armor. Breach scales armor effectiveness by `-0.15` per level.
fn dmg_after_breach(c: &Client, raw: f32, kind: u8, breach_level: i64) -> f32 {
    let factor = armor_dmg_factor(c, raw);
    let effectiveness = (1.0 - 0.15 * breach_level.clamp(0, 4) as f64).max(0.0);
    let armor_reduction = ((1.0 - factor) * effectiveness).clamp(0.0, 1.0);
    let arm = f64::from(raw) * (1.0 - armor_reduction);
    let resisted = arm * resistance_damage_factor(c);
    (resisted * (1.0 - 0.04 * enchant_epf(c, kind) as f64)) as f32
}

fn wind_burst_bounce_velocity(level: i64) -> f64 {
    // `wind_burst.json` lookup: I/II/III are exactly 1.2, 1.75 and 2.2.
    match level {
        1 => 1.2,
        2 => 1.75,
        3.. => 2.2,
        _ => 0.0,
    }
}

fn mace_attack_fall(me: &Client, item: i64) -> f32 {
    if item_to_name(item) != Some("mace") {
        return 0.0;
    }
    let (_, y, _, _, _, grounded) = *lk(&me.pos);
    if grounded || *lk(&me.fall_fly) {
        return 0.0;
    }
    (*lk(&me.fall_from) - y) as f32
}

/// Called after the victim accepts damage. Reset the fall immediately so a
/// second attack before the next combat tick cannot reuse it.
fn queue_mace_smash(me: &Client, held: &Stack, fall: f32, target: i32, center: [f64; 3]) {
    if fall <= 1.5 {
        return;
    }
    let (x, y, z, ..) = *lk(&me.pos);
    wind_charge_reset_fall(me, y);
    let smash = MaceSmash {
        dim: *lk(&me.dim),
        target,
        center,
        origin: [x, y, z],
        level: enchant_level(&held.2, 41),
        fall,
    };
    lk(mace_smashes()).insert(me.eid, smash);
}

/// Vanilla `MaceItem.getKnockbackPower`: `(3.5 - distance) * 0.7 * heavy *
/// (1 - generic.knockback_resistance)` over the 3D feet-to-feet distance, with
/// the push `(dir.x * power, 0.7, dir.z * power)` — the vertical component is
/// a constant and never sees the resistance factor.
fn mace_smash_impulse(
    center: [f64; 3],
    pos: [f64; 3],
    heavy: f64,
    knockback_resistance: f64,
) -> Option<[f64; 3]> {
    let dx = pos[0] - center[0];
    let dy = pos[1] - center[1];
    let dz = pos[2] - center[2];
    let distance = dx.hypot(dy).hypot(dz);
    let power = (3.5 - distance) * 0.7 * heavy * (1.0 - knockback_resistance);
    if power <= 0.0 {
        return None;
    }
    let length = distance.max(0.01);
    Some([dx / length * power, 0.7, dz / length * power])
}

/// Vanilla `Explosion.hurtEntities` knockback for the mace Wind Burst
/// enchantment: a knockback-only burst of radius 3.5 centered on the
/// attacker. `d14 = feet_distance / 3.5` gates inclusion (`<= 1`), the push
/// direction is the normalized `(dx, eye_y - cy, dz)` delta, and the strength
/// is `(1 - d14) * level_multiplier * (1 -
/// generic.explosion_knockback_resistance)` — every entity in range,
/// attacker and victim included. `seenPercent` is approximated as full, the
/// same simplification the wind-charge burst path already uses.
fn wind_burst_explosion_impulse(
    center: [f64; 3],
    target_pos: [f64; 3],
    target_eye_y: f64,
    level_multiplier: f64,
    explosion_knockback_resist: f64,
) -> Option<[f64; 3]> {
    const RADIUS: f64 = 3.5;
    let dx = target_pos[0] - center[0];
    let dz = target_pos[2] - center[2];
    let feet = (dx * dx + (target_pos[1] - center[1]).powi(2) + dz * dz).sqrt();
    if feet / RADIUS > 1.0 {
        return None;
    }
    let dy = target_eye_y - center[1];
    let len = (dx * dx + dy * dy + dz * dz).sqrt();
    if len <= 1e-9 || !level_multiplier.is_finite() {
        return None;
    }
    let strength =
        (1.0 - feet / RADIUS) * level_multiplier * (1.0 - explosion_knockback_resist.clamp(0.0, 1.0));
    Some([dx / len * strength, dy / len * strength, dz / len * strength])
}

/// Record `impulse` as the mob's `deltaMovement` and move it through the same
/// collision sweep other entity motion uses, then broadcast the resulting
/// pose to tracking clients.
fn mob_apply_knockback_impulse(m: &Arc<Mob>, impulse: [f64; 3]) {
    let (x, y, z, yaw, pitch) = {
        let mut pos = lk(&m.pos);
        {
            let mut vel = lk(&m.vel);
            vel[0] += impulse[0];
            vel[1] += impulse[1];
            vel[2] += impulse[2];
        }
        let (width, height) = mob_dimensions(m.etype);
        let on_ground = mob_has_support_dim(m.dim, m.etype, pos.0, pos.1, pos.2);
        let (next, _) = step_entity_move_dim(
            m.dim,
            [pos.0, pos.1, pos.2],
            width,
            height,
            impulse,
            on_ground,
            0.6,
        );
        pos.0 = next[0];
        pos.1 = next[1];
        pos.2 = next[2];
        *pos
    };
    broadcast_seen(m.eid, |_| (0x70, entity_teleport(m.eid, x, y, z, yaw, pitch, true)));
}

fn mace_smash_pet_exempt(mob: &Arc<Mob>, attacker_uuid: [u8; 16]) -> bool {
    lk(live_mob_states())
        .get(&mob.eid)
        .and_then(|state| state.behavior.as_ref())
        .is_some_and(|behavior| match behavior {
            MobBehaviorState::Wolf(pet) | MobBehaviorState::Cat(pet) => {
                pet.owner == Some(attacker_uuid)
            }
            _ => false,
        })
}

fn wind_burst_apply(c: &Arc<Client>, smash: MaceSmash) {
    let heavy = if smash.fall > 5.0 { 2.0 } else { 1.0 };
    let attacker_uuid = c.uuid;
    let attacker_team = p3b_team_of(&c.name);
    let nearby: Vec<_> = lk(mobs()).iter().cloned().collect();
    for m in nearby {
        if m.dim != smash.dim
            || m.eid == smash.target
            || mace_smash_pet_exempt(&m, attacker_uuid)
            || *lk(&m.hp) <= 0.0
        {
            continue;
        }
        let (x, y, z, ..) = *lk(&m.pos);
        let Some(impulse) = mace_smash_impulse(smash.center, [x, y, z], heavy, 0.0) else {
            continue;
        };
        // Record vanilla `deltaMovement`, then move this tick's impulse
        // through the same collision sweep other entity motion uses. The
        // old radial pulse rewrote the position directly, so a mob could
        // be shoved through a wall.
        mob_apply_knockback_impulse(&m, impulse);
    }
    // Nearby players are living entities for the radial pulse too. They receive
    // a velocity packet rather than a position rewrite; the attacker's own Wind
    // Burst launch below remains the only impulse the wielder gets.
    for player in client_snapshot() {
        if player.eid == c.eid
            || player.eid == smash.target
            || *lk(&player.dim) != smash.dim
            || player.is_spectator()
            || *lk(&player.hp) <= 0.0
            || (attacker_team.is_some()
                && attacker_team == p3b_team_of(&player.name))
        {
            continue;
        }
        let (x, y, z, ..) = *lk(&player.pos);
        let Some(impulse) = mace_smash_impulse(
            smash.center,
            [x, y, z],
            heavy,
            knockback_resist_of(&player),
        ) else {
            continue;
        };
        let packet = velocity_pkt(
            player.eid,
            (impulse[0] * 8000.0) as i16,
            (impulse[1] * 8000.0) as i16,
            (impulse[2] * 8000.0) as i16,
        );
        if let Ok(mut sock) = player.sock.lock() {
            let _ = send_packet(&mut sock, 0x5a, packet.clone());
        }
        broadcast_seen_ex(player.eid, player.eid, |_| (0x5a, packet.clone()));
    }
    let [x, y, z] = smash.center;
    let mut event = 2013i32.to_be_bytes().to_vec();
    write_pos(&mut event, x.floor() as i32, y.floor() as i32, z.floor() as i32);
    event.extend_from_slice(&750i32.to_be_bytes());
    event.push(0);
    broadcast_near_packet(smash.dim, x, y, z, -1, 0x28, &event);
    if smash.level > 0 {
        // Vanilla `WindBurstEnchantment`: a knockback-only explosion of radius
        // 3.5 centered on the attacker, hitting every entity in range —
        // attacker and victim included — scaled by the per-level multiplier.
        let multiplier = wind_burst_bounce_velocity(smash.level);
        let burst_mobs: Vec<_> = lk(mobs()).iter().cloned().collect();
        for m in burst_mobs {
            if m.dim != smash.dim || *lk(&m.hp) <= 0.0 {
                continue;
            }
            let (mx, my, mz, ..) = *lk(&m.pos);
            let eye = my + f64::from(entity_eye_height_1211(m.etype));
            let Some(impulse) = wind_burst_explosion_impulse(
                smash.origin,
                [mx, my, mz],
                eye,
                multiplier,
                0.0,
            ) else {
                continue;
            };
            mob_apply_knockback_impulse(&m, impulse);
        }
        for player in client_snapshot() {
            if *lk(&player.dim) != smash.dim
                || player.is_spectator()
                || *lk(&player.hp) <= 0.0
            {
                continue;
            }
            let (px, py, pz, ..) = *lk(&player.pos);
            let eye = client_eye_y(&player, smash.dim, px, py, pz);
            let Some(impulse) = wind_burst_explosion_impulse(
                smash.origin,
                [px, py, pz],
                eye,
                multiplier,
                explosion_knockback_resist_of(&player),
            ) else {
                continue;
            };
            let packet = velocity_pkt(
                player.eid,
                (impulse[0] * 8000.0) as i16,
                (impulse[1] * 8000.0) as i16,
                (impulse[2] * 8000.0) as i16,
            );
            if let Ok(mut sock) = player.sock.lock() {
                let _ = send_packet(&mut sock, 0x5a, packet.clone());
            }
            broadcast_seen_ex(player.eid, player.eid, |_| (0x5a, packet.clone()));
        }
        let power = wind_burst_bounce_strength(smash.level as i32, smash.fall, false);
        let [x, y, z] = smash.origin;
        let pulse = explosion_pkt_with_power(x, y, z, power, &[]);
        broadcast_near_packet(smash.dim, x, y, z, -1, 0x20, &pulse);
    }
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
    let mut raised = lk(shield_raised());
    for c in &cls {
        live.insert(c.eid);
        if *lk(&c.blocking) {
            raised.entry(c.eid).or_insert(now);
        } else {
            raised.remove(&c.eid);
        }
    }
    raised.retain(|eid, _| live.contains(eid));
    drop(raised);
    lk(riptide_spin())
        .retain(|eid, _| live.contains(eid));
    {
        // Deliver the knockback and Wind Burst from accepted hits.
        let mut smashes = lk(mace_smashes());
        let mut drained = Vec::new();
        for c in &cls {
            if let Some(smash) = smashes.remove(&c.eid) {
                drained.push((Arc::clone(c), smash));
            }
        }
        smashes.retain(|eid, _| live.contains(eid));
        drop(smashes);
        for (c, smash) in drained {
            if *lk(&c.dim) == smash.dim {
                wind_burst_apply(&c, smash);
            }
        }
    }
    {
        let live_mobs: HashSet<i32> = lk(mobs()).iter().map(|mob| mob.eid).collect();
        let mut last_hurt = lk(mob_last_hurt_amount());
        last_hurt.retain(|eid, _| live_mobs.contains(eid));
        bound_mob_last_hurt_amount(&mut last_hurt);
        drop(last_hurt);
        let mut effects = lk(mob_effects());
        effects.retain(|eid, _| live_mobs.contains(eid));
        bound_mob_effects(&mut effects);
    }
    spin_attack_step();
    wind_charge_projectile_step();
    fireworks_step();
}

/// `LivingEntity.aiStep`'s auto-spin tick: one spin tick per server tick for
/// every player a Riptide release armed. The swept box is the box the player
/// held at the previous tick joined with the current one — vanilla calls
/// `checkAutoSpinAttack(aabb, getBoundingBox())` — and the first living mob it
/// catches takes `autoSpinAttackDmg` (8.0) through the same armor/resistance
/// pipeline as a melee hit, is pushed away from the spinner, and ends the spin.
fn spin_attack_step() {
    let now = TICK_IDX.load(Ordering::Relaxed);
    let expired: Vec<i32> = {
        let mut spins = lk(riptide_spin());
        let expired: Vec<i32> = spins
            .iter()
            .filter(|(_, until)| **until <= now)
            .map(|(eid, _)| *eid)
            .collect();
        for eid in &expired {
            spins.remove(eid);
        }
        expired
    };
    // A finished spin clears `DATA_LIVING_ENTITY_FLAGS` bit 4 once.
    if !expired.is_empty() {
        for c in client_snapshot() {
            if expired.contains(&c.eid) {
                broadcast_spin_flags(&c, false);
            }
        }
    }
    let armed: Vec<Arc<Client>> = {
        let spins = lk(riptide_spin());
        client_snapshot()
            .into_iter()
            .filter(|c| spins.contains_key(&c.eid))
            .collect()
    };
    if armed.is_empty() {
        return;
    }
    let live_mobs = mobs().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut mob_dead: Vec<(i32, i64, f64, f64, f64)> = Vec::new();
    for spinner in armed {
        let dim = *lk(&spinner.dim);
        let (px, py, pz, ..) = *lk(&spinner.pos);
        let (lx, ly, lz) = *lk(&spinner.lastp);
        let (half_w, height) = if *lk(&spinner.sneaking) {
            (0.3, 1.5)
        } else {
            (0.3, 1.8)
        };
        let swept_min = [px.min(lx) - half_w, py.min(ly), pz.min(lz) - half_w];
        let swept_max = [
            px.max(lx) + half_w,
            py.max(ly) + height,
            pz.max(lz) + half_w,
        ];
        let mut hit = false;
        for m in live_mobs.iter().filter(|m| m.dim == dim) {
            // `iter().filter()` hands out `&&Arc<Mob>`; one coercion keeps the
            // helpers below on their `&Mob`/`&Arc<Mob>` contracts.
            let m: &Arc<Mob> = m;
            let (mx, my, mz, ..) = *lk(&m.pos);
            let (mw, mh) = entity_dimensions_1211(m.etype);
            let half = f64::from(mw) / 2.0;
            let inside = swept_min[0] <= mx + half
                && swept_max[0] >= mx - half
                && swept_min[1] <= my + f64::from(mh)
                && swept_max[1] >= my
                && swept_min[2] <= mz + half
                && swept_max[2] >= mz - half;
            if !inside || !live_mob_hit_allowed(m, spinner.eid, BossHit::Other) {
                continue;
            }
            let Some(accepted) = mob_melee_hurt_gate(m, TRIDENT_SPIN_DAMAGE) else {
                continue;
            };
            let damage = mob_melee_mitigate(m, accepted, 0);
            let mut health = lk(&m.hp);
            if *health <= 0.0 {
                continue;
            }
            *health -= damage;
            let dead = *health <= 0.0;
            drop(health);
            // `LivingEntity.knockback(0.4, dx, dz)` away from the spinner.
            let (dx, dz) = (mx - px, mz - pz);
            let len = (dx * dx + dz * dz).sqrt().max(1e-9);
            mob_apply_knockback_impulse(m, [dx / len * 0.4, 0.4, dz / len * 0.4]);
            if dead {
                mob_dead.push((m.eid, m.etype, mx, my, mz));
            }
            hit = true;
            break;
        }
        if hit {
            lk(riptide_spin()).remove(&spinner.eid);
            broadcast_spin_flags(&spinner, false);
        }
    }
    for (deid, etype, mx, my, mz) in mob_dead {
        kill_mob(deid, etype, mx, my, mz);
    }
}

/// Move each in-flight firework and detonate it when its component-derived
/// fuse expires. Vanilla firework explosions damage entities only — they do
/// not break blocks.
fn fireworks_step() {
    let mut expired = Vec::new();
    {
        let mut fws = lk(fireworks());
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
    for (eid, owner, x, y, z, explosions) in expired {
        let fdim = entity_dim_of(eid);
        clear_entity_dim(eid);
        firework_explode(fdim, x, y, z, explosions, owner);
        broadcast_seen_destroy(eid);
    }
}

fn firework_damage_base(explosions: i64) -> f32 {
    // 1.21.1 bytecode adds two damage per star to the five-damage base.
    (5.0 + 2.0 * explosions.max(0) as f64) as f32
}

/// Vanilla `FireworkRocketEntity.explode`: `5 + 2 * stars` damage inside
/// a five-block radius, scaled by distance and requiring collision visibility
/// to the feet or midpoint. Fireworks never break blocks.
fn firework_explode(dim: DimId, x: f64, y: f64, z: f64, explosions: i64, owner: i32) {
    // `FireworkRocketEntity.explode` uses `ExplosionInteraction.NONE`, i.e. the
    // `KEEP` block interaction: no cells are removed and the client draws the
    // small particle whatever the power.
    let packet = explosion_pkt_with_power_and_interaction(
        x,
        y,
        z,
        1.0,
        &[],
        EXPLOSION_BLOCK_INTERACTION_KEEP,
    );
    broadcast_near(dim, x, y, z, -1, |_c| (0x20, packet.clone()));
    if explosions <= 0 {
        return;
    }
    let base_power = firework_damage_base(explosions);
    let firework_visible = |target: [f64; 3], height: f64| {
        [0.0, 0.5].into_iter().any(|part| {
            !collider_ray_hit(
                dim,
                [x, y, z],
                [target[0], target[1] + height * part, target[2]],
            )
        })
    };
    let scaled_damage = |distance: f64| {
        base_power * ((5.0 - distance) / 5.0).max(0.0).sqrt() as f32
    };
    for cl in client_snapshot() {
        if *lk(&cl.dim) != dim {
            continue;
        }
        let (px, py, pz, ..) = *lk(&cl.pos);
        let dd = ((px - x).powi(2) + (py - y).powi(2) + (pz - z).powi(2)).sqrt();
        // Vanilla hurts the shooter for the unscaled star damage first, then
        // excludes that entity from the nearby-entity sweep.
        let direct_owner = cl.eid == owner;
        if (direct_owner
            || (dd < 5.0 && firework_visible([px, py, pz], 1.8)))
            && !matches!(*lk(&cl.gm), 1 | 3)
        {
            let raw = if direct_owner { base_power } else { scaled_damage(dd) };
            // Mod-suite `EntityDamagePre`: a cancelled/rewritten hit skips
            // or reshapes this player's share of the blast.
            let Some(raw) = live_mod_entity_damage_pre(raw) else {
                continue;
            };
            let Some((accepted, _)) = damage_after_invulnerability(&cl, raw) else {
                continue;
            };
            let source = if owner >= 0 {
                DamageSource::new("fireworks").with_cause(owner)
            } else {
                DamageSource::new("fireworks")
            };
            player_hurt_apply(&cl, accepted, &source, Some(raw));
        }
    }
    for mob in lk(mobs()).iter() {
        if mob.dim != dim {
            continue;
        }
        let (mx, my, mz, ..) = *lk(&mob.pos);
        let dd = ((mx - x).powi(2) + (my - y).powi(2) + (mz - z).powi(2)).sqrt();
        let (_, mob_height) = entity_dimensions_1211(mob.etype);
        let direct_owner = mob.eid == owner;
        if direct_owner || (dd < 5.0 && firework_visible([mx, my, mz], f64::from(mob_height))) {
            let amount = if direct_owner { base_power } else { scaled_damage(dd) };
            // Mod-suite `EntityDamagePre` for blast-damaged mobs.
            let Some(dmg) = live_mod_entity_damage_pre(amount) else {
                continue;
            };
            let Some(accepted) = mob_melee_hurt_gate(mob, dmg) else {
                continue;
            };
            let dmg = mob_melee_mitigate(mob, accepted, 0);
            let mut hp = lk(&mob.hp);
            *hp = (*hp - dmg).max(0.0);
        }
    }
}

/// Per-mob age for `wind_charge`/`breeze_wind_charge` projectile entities.
fn wind_charge_ages() -> &'static Mutex<HashMap<i32, u16>> {
    static AGES: OnceLock<Mutex<HashMap<i32, u16>>> = OnceLock::new();
    AGES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Wind-charge projectiles (`wind_charge`, `breeze_wind_charge`) spawned by
/// `/summon` or a spawn egg live in the mob store: they are entities, not AI
/// mobs, and the generic velocity pass already moves them. Burst each charge
/// on the first solid-block contact or after its vanilla 200-tick lifetime.
fn wind_charge_projectile_step() {
    let player_charge = name_to_etype("wind_charge");
    let breeze_charge = name_to_etype("breeze_wind_charge");
    let Some(reg) = breg() else {
        return;
    };
    let mut bursts = Vec::new();
    {
        let mut ages = lk(wind_charge_ages());
        let mut seen = HashSet::new();
        for mob in lk(mobs()).iter() {
            let kind = if Some(mob.etype) == player_charge {
                WindChargeKind::Player
            } else if Some(mob.etype) == breeze_charge {
                WindChargeKind::Breeze
            } else {
                continue;
            };
            let (mx, my, mz, ..) = *lk(&mob.pos);
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
                *lk(&mob.hp) = 0.0;
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
    lk(shield_raised())
        .get(&c.eid)
        .map_or(0, |since| {
            now.saturating_sub(*since) * u64::from(GAME_TICKS_PER_TICK)
        })
}

/// The spin-attack window a Riptide launch armed for `eid`, in game ticks.
#[cfg(all(test, megu_vanilla_data))]
fn spin_attack_ticks_left(eid: i32) -> u64 {
    let now = TICK_IDX.load(Ordering::Relaxed);
    lk(riptide_spin())
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
        let p = lk(&c.pos);
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
        cooldown_active: lk(&c.shield_cd).elapsed() <= SHIELD_DISABLE_DUR,
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
    *lk(&c.blocking) = false;
    *lk(&c.shield_cd) = Instant::now();
}

// ---------- totem of undying ----------

/// Vanilla `DamageTypeTags.BYPASSES_INVULNERABILITY` splits sources that can
/// trigger `checkTotemDeathProtection` from `fellOutOfWorld`/`generic_kill`,
/// which always bypass it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TotemDamageKind {
    Ordinary,
    Void,
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
        || *lk(&c.gm) == 1
    {
        return false;
    }
    let mut inv = lk(&c.inv);
    let held_k = 36i16 + (*lk(&c.held));
    for k in [45i16, held_k] {
        if let Some(st) = inv.get(&k).cloned().filter(|s| s.0 == 1163) {
            if st.1 <= 1 {
                inv.remove(&k);
            } else {
                inv.insert(k, (st.0, st.1 - 1, st.2));
            }
            let now = inv.get(&k).cloned().unwrap_or((0, 0, Vec::new()));
            drop(inv);
            *lk(&c.hp) = 1.0;
            resync_slot(c, 0, k, &now);
            // entity_status 35 = totem use; i32 entity id + i8 status
            let mut st_pkt = Vec::new();
            st_pkt.extend_from_slice(&c.eid.to_be_bytes());
            st_pkt.push(35);
            let _ = send_packet(&mut lk(&c.sock), 0x1f, st_pkt.clone());
            broadcast_seen(c.eid, |_c| (0x1f, st_pkt.clone()));
            let (px, py, pz, ..) = {
                let p = lk(&c.pos);
                *p
            };
            let snd = sound_pkt_named("item.totem.use", px, py, pz);
            broadcast_seen(c.eid, |_c| (0x68, snd.clone()));
            // Death protection replaces existing effects with the vanilla grant set.
            grant_totem_effects(c, &mut lk(&c.effs));
            let _ = send_packet(&mut lk(&c.sock), 0x5d, health_pkt_c(1.0, c));
            return true;
        }
    }
    false
}

// ---------- trident specials ----------

/// `TridentItem` durability ceiling (`MAX_DAMAGE = 250` in the 1.21.1 item
/// table) and `isTooDamagedToUse`'s gate: `damage >= MAX_DAMAGE - 1` refuses
/// both the throw (`releaseUsing` returns before the projectile branch) and the
/// right-click (`use` returns `InteractionResult.FAIL`).
const TRIDENT_MAX_DAMAGE: i64 = 250;
/// `TridentItem.getMaxStackSize` is the item default of 1 — a trident never
/// stacks, so every path that puts one back in an inventory or on the ground
/// writes exactly this count.
const TRIDENT_STACK_LIMIT: i64 = 1;
/// `startAutoSpinAttack(20, 8.0F, stack)`: the spin lives 20 game ticks and
/// `Player.attack` pays `autoSpinAttackDmg` for each living entity the swept
/// box catches.
const TRIDENT_SPIN_TICKS: u32 = 20;
const TRIDENT_SPIN_DAMAGE: f32 = 8.0;
/// `LivingEntity.DATA_LIVING_ENTITY_FLAGS` bit 4 (`LIVING_ENTITY_FLAG_SPIN_ATTACK`).
const LIVING_FLAG_SPIN_ATTACK: u8 = 0x04;
/// `net.minecraft.world.entity.LivingEntity.DATA_LIVING_ENTITY_FLAGS` — the
/// `Byte` at class-tree index 8 (`proto/src/entity_metadata.rs`,
/// `living_entity::LIVING_FLAGS`).
const META_INDEX_LIVING_ENTITY_FLAGS: u8 = 8;
/// `SoundSource.PLAYERS` — the source `TridentItem.releaseUsing` passes for the
/// throw and riptide release sounds.
const SOUND_SOURCE_PLAYERS: u32 = 7;

/// `TridentItem.isTooDamagedToUse`: a trident at `MAX_DAMAGE - 1` damage is
/// spent and refuses every use.
fn trident_too_damaged(comps: &Comps) -> bool {
    comp_varint(comps, COMP_DAMAGE).unwrap_or(0) >= TRIDENT_MAX_DAMAGE - 1
}

/// `ThrownTrident.onHitEntity`'s base damage: vanilla's `8.0F` literal.
/// `play_protocol` bakes an unconditional `2.5 * Impaling` term into the
/// projectile's `dmg` at throw time, which vanilla gates on
/// `#minecraft:sensitive_to_impaling`; taking that term back out here is what
/// makes both callers below exact.
fn thrown_trident_base_damage(a: &Arrow) -> f32 {
    let unconditional =
        TYPED_ENCHANT_DAMAGE_PER_LEVEL * enchant_level(&a.ret_comps, ENCHANT_IMPALING) as f32;
    (a.dmg - unconditional).max(0.0)
}

/// The damage a thrown trident deals to a victim of type `etype` (`None` for a
/// player, which is never in the impaling tag): the base plus every type-gated
/// enchantment term on the trident's own components.
fn thrown_trident_hit_damage(a: &Arrow, etype: Option<i64>) -> f32 {
    thrown_trident_base_damage(a)
        + etype.map_or(0.0, |t| typed_damage_bonus(&a.ret_comps, t))
}

/// `ThrownTrident.tick`'s return whoosh: `playSound(TRIDENT_RETURN, 10.0F,
/// 1.0F)` from the entity's own `SoundSource.NEUTRAL`, once per return.
fn trident_return_whoosh(a: &Arrow) {
    let snd = sound_pkt_named_ex(
        "item.trident.return",
        SOUND_SOURCE_NEUTRAL,
        a.x,
        a.y,
        a.z,
        10.0,
        1.0,
    );
    broadcast_seen(a.eid, |_c| (0x68, snd.clone()));
}

/// `set_entity_data` (0x58) body for `LivingEntity.DATA_LIVING_ENTITY_FLAGS`, a
/// `Byte` at index 8.
fn living_entity_flags_pkt(eid: i32, flags: u8) -> Vec<u8> {
    encode_set_entity_data(
        eid,
        &[MetaEntry::new(
            META_INDEX_LIVING_ENTITY_FLAGS,
            MetaValue::Byte(flags),
        )],
    )
}

/// `startAutoSpinAttack`'s flag sync: vanilla writes the living-entity flag bit
/// server-side and lets the tracker pass carry it, so every watcher plus the
/// spinner itself sees the spinning pose.
fn broadcast_spin_flags(c: &Client, spinning: bool) {
    let flags = if spinning { LIVING_FLAG_SPIN_ATTACK } else { 0 };
    let pkt = living_entity_flags_pkt(c.eid, flags);
    broadcast_seen_packet(c.eid, 0x58, &pkt);
    if let Ok(mut sock) = c.sock.lock() {
        let _ = send_packet(&mut sock, 0x58, pkt);
    }
}

/// `TridentItem.releaseUsing`'s riptide impulse magnitude: the enchantment's
/// `minecraft:trident_spin_attack_strength` value, an `add`/`linear` effect with
/// `base 1.5` and `per_level_above_first 0.75`
/// (`data/minecraft/enchantment/riptide.json`), i.e. `3.0 * (1 + level) / 4.0`.
fn riptide_strength(level: i64) -> f64 {
    3.0 * (1.0 + level as f64) / 4.0
}

/// `evaluate_trident_special` inputs: enchant levels read off the thrown
/// trident (Loyalty 19 / Riptide 31 / Channeling 5) and the situation flags
/// each vanilla branch reads.
#[derive(Clone, Copy, Debug, Default)]
struct TridentSpecialCtx {
    loyalty: i64,
    riptide: i64,
    channeling: i64,
    /// `TridentItem.isTooDamagedToUse` on the thrown stack.
    too_damaged: bool,
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
    /// `push(look.normalize() * riptide_strength)` — vanilla scales the unit
    /// look vector by the enchantment's spin-attack-strength value.
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
    } else if ctx.too_damaged || !ctx.wet || !ctx.look.iter().all(|v| v.is_finite()) {
        RiptideOutcome::Refused
    } else {
        let n = (ctx.look[0] * ctx.look[0] + ctx.look[1] * ctx.look[1] + ctx.look[2] * ctx.look[2])
            .sqrt();
        if n <= 1e-9 {
            RiptideOutcome::Refused
        } else {
            let speed = riptide_strength(ctx.riptide);
            RiptideOutcome::Launch(RiptideLaunch {
                velocity: [
                    ctx.look[0] / n * speed,
                    ctx.look[1] / n * speed,
                    ctx.look[2] / n * speed,
                ],
                spin_ticks: TRIDENT_SPIN_TICKS,
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
        let p = lk(&c.pos);
        *p
    };
    let dim = *lk(&c.dim);
    let sneaking = *lk(&c.sneaking);
    let fall_fly = *lk(&c.fall_fly);
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
    let hs = 36 + *lk(&c.held);
    {
        let mut inv = lk(&c.inv);
        if let std::collections::hash_map::Entry::Vacant(e) = inv.entry(hs) {
            e.insert((item, TRIDENT_STACK_LIMIT as u8, comps.clone()));
            let st = inv.get(&hs).cloned().unwrap_or((0, 0, Vec::new()));
            drop(inv);
            resync_slot(c, 0, hs, &st);
            return;
        }
    }
    let (x, y, z, ..) = {
        let p = lk(&c.pos);
        *p
    };
    lk(drops())
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
        let mut inv = lk(&me.inv);
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
        *lk(&me.armor_pts) = armor_pts_of(&inv);
    }
    if broke {
        let (x, y, z, ..) = {
            let p = lk(&me.pos);
            *p
        };
        let snd = sound_pkt_named("entity.item.break", x, y + 1.0, z);
        broadcast(-1, |_c| (0x68, snd.clone()));
    }
    // resync the four armor slots (cheap: keeps client honest)
    for slot in [5i16, 6, 7, 8] {
        let st = lk(&me.inv)
            .get(&slot)
            .cloned()
            .unwrap_or((0, 0, Vec::new()));
        let mut p = vec![0u8];
        write_varint(&mut p, i64::from(inv_state_next(me)));
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

/// `Explosion.BlockInteraction` ordinals in the pinned jar's declaration order:
/// `KEEP`, `DESTROY`, `DESTROY_WITH_DECAY`, `TRIGGER_BLOCK`. The wire carries
/// the ordinal, so a shifted value silently changes what the client breaks and
/// which particle it draws (`radius >= 2` picks the large one only when the
/// interaction is not `KEEP`).
const EXPLOSION_BLOCK_INTERACTION_KEEP: i64 = 0;
const EXPLOSION_BLOCK_INTERACTION_DESTROY_WITH_DECAY: i64 = 2;
/// `Level.getDestroyType(ExplosionInteraction.TRIGGER)` — wind charges and
/// Wind Burst, which never remove blocks but still trigger the blast.
const EXPLOSION_BLOCK_INTERACTION_TRIGGER_BLOCK: i64 = 3;
/// `ParticleTypes` ids in the 1.21.1 client registry order. `Explosion`'s
/// default constructor passes `EXPLOSION` (22) as the small option and
/// `EXPLOSION_EMITTER` (21) as the large one; the client picks between them on
/// `radius >= 2`.
const PARTICLE_EXPLOSION: i64 = 22;
const PARTICLE_EXPLOSION_EMITTER: i64 = 21;
/// `Holder<SoundEvent>` encoding for `entity.generic.explode`:
/// `BuiltInRegistries.SOUND_EVENT` id 576 + 1 (a `Holder.Reference` writes
/// `registryId + 1`; `0` would start an inline event instead).
const SOUND_EVENT_GENERIC_EXPLODE_HOLDER: i64 = 577;

/// `ClientboundExplodePacket` (S2C 0x20 in protocol 767 — one above the
/// `0x1f` Entity Event this binary already sends) carries the blast centre,
/// the destroyed-cell offsets and the client-side presentation. Field order and
/// types follow the pinned jar's `write(RegistryFriendlyByteBuf)`:
/// `x/y/z` doubles, `power` float, `VarInt` count plus three signed bytes per
/// cell, the per-viewer knockback as three floats, then the block-interaction
/// enum ordinal, the small and large `ParticleOptions` registry ids, and the
/// `Holder<SoundEvent>`.
fn explosion_pkt_with_power_and_interaction(
    x: f64,
    y: f64,
    z: f64,
    power: f32,
    records: &[(i8, i8, i8)],
    block_interaction: i64,
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
    write_varint(&mut p, block_interaction);
    write_varint(&mut p, PARTICLE_EXPLOSION); // small: `explosion`
    write_varint(&mut p, PARTICLE_EXPLOSION_EMITTER); // large: `explosion_emitter`
    write_varint(&mut p, SOUND_EVENT_GENERIC_EXPLODE_HOLDER);
    p
}

/// The `ExplosionInteraction.TRIGGER` form (`TRIGGER_BLOCK`): wind charges and
/// the mace's Wind Burst never remove blocks but still show the vanilla blast.
/// Callers that do remove blocks pass their own interaction through
/// [`explosion_pkt_with_power_and_interaction`].
fn explosion_pkt_with_power(
    x: f64, y: f64, z: f64, power: f32, records: &[(i8, i8, i8)],
) -> Vec<u8> {
    explosion_pkt_with_power_and_interaction(
        x,
        y,
        z,
        power,
        records,
        EXPLOSION_BLOCK_INTERACTION_TRIGGER_BLOCK,
    )
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
/// Shared post-death loot scatter: `mob_loot` gated on the mob-loot gamerule;
/// `was_sheared` suppresses the sheared-mutton stack (player kills only).
fn mob_death_drops(dim: DimId, etype: i64, pos: [f64; 3], burning: bool, was_sheared: bool) {
    for (item, cnt, comps) in mob_loot(etype, burning)
        .into_iter()
        .filter(|st| !(was_sheared && st.0 == 202))
        .filter(|_| GR_MOB_LOOT.load(Ordering::Relaxed) == 1)
    {
        drop_spawn_scattered(dim, pos[0], pos[1], pos[2], item, cnt, comps, 0, None, None);
    }
}

fn kill_mob(deid: i32, etype: i64, mx: f64, my: f64, mz: f64) {
    extinguish_mob_fire(deid);
    let Some((mob, state)) = remove_live_mob(deid) else { return };
    let burning = *lk(&mob.fire_ticks) > 0;
    live_mob_death(&mob, state, [mx, my, mz]);
    broadcast_seen_destroy(deid);
    let was_sheared = lk(sheared()).remove(&deid);
    mob_death_drops(mob.dim, etype, [mx, my, mz], burning, was_sheared);
    xp_spawn_dim(mob.dim, mx, my, mz, mob_xp(etype));
    // spawn streams via interest_tick
}

fn mob_env_die(deid: i32, etype: i64, mx: f64, my: f64, mz: f64, burning: bool) {
    extinguish_mob_fire(deid);
    // environmental death (suffocation/drowning/fall): drops but no XP —
    // vanilla only drops XP when recently hurt by a player
    let Some((mob, state)) = remove_live_mob(deid) else { return };
    live_mob_death(&mob, state, [mx, my, mz]);
    broadcast_seen_destroy(deid);
    lk(sheared()).remove(&deid);
    mob_death_drops(mob.dim, etype, [mx, my, mz], burning, false);
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
const MAX_MOB_LOOT_ATTACKER_TYPES: usize = 256;
type MobLootAttackers = HashMap<i64, (i32, String, u64)>;
static MOB_LOOT_ATTACKERS: OnceLock<Mutex<MobLootAttackers>> = OnceLock::new();
fn mob_loot_attackers() -> &'static Mutex<MobLootAttackers> {
    MOB_LOOT_ATTACKERS.get_or_init(|| Mutex::new(MobLootAttackers::new()))
}
fn record_mob_loot_attacker(etype: i64, source: i32) {
    if source <= 0 {
        return;
    }
    let Some(client) = lk(clients())
        .iter()
        .find(|client| client.eid == source)
        .cloned()
    else {
        return;
    };
    let held_slot = *lk(&client.held);
    let held_item = lk(&client.inv)
        .get(&(36 + held_slot))
        .map(|(item, _, _)| *item);
    let held_name = held_item.and_then(|item| item_to_name(item).map(str::to_string));
    let mut attackers = lk(mob_loot_attackers());
    if attackers.len() >= MAX_MOB_LOOT_ATTACKER_TYPES && !attackers.contains_key(&etype) {
        attackers.clear();
    }
    attackers.insert(
        etype,
        (source, held_name.unwrap_or_default(), TICK_IDX.load(Ordering::Relaxed)),
    );
}

fn mob_loot_table_id(etype: i64) -> String {
    ENTITY_REGISTRY_1211
        .get(usize::try_from(etype).unwrap_or(usize::MAX))
        .map(|spec| format!("minecraft:entities/{}", spec.name))
        .unwrap_or_else(|| format!("minecraft:entities/{etype}"))
}

fn mob_loot(etype: i64, burning: bool) -> Vec<Stack> {
    let drops = mob_loot_base(etype, burning);
    let attacker = lk(mob_loot_attackers())
        .get(&etype)
        .cloned();
    let now = TICK_IDX.load(Ordering::Relaxed);
    let killed_by_player = attacker.as_ref().is_some_and(|(source, _, tick)| {
        *source > 0 && now.saturating_sub(*tick) <= 100
    });
    let held_tool = attacker
        .filter(|(_, _, tick)| now.saturating_sub(*tick) <= 100)
        .map(|(_, held, _)| held)
        .filter(|held| !held.is_empty());
    crate::apply_live_mod_loot_modifiers(
        &mob_loot_table_id(etype),
        DimId::Overworld,
        None,
        killed_by_player,
        held_tool.as_deref(),
        drops,
        now,
    )
}

fn mob_loot_base(etype: i64, burning: bool) -> Vec<Stack> {
    let raw: Vec<Stack> = if let Some(pools) = lk(mob_loot_tables()).get(&etype).map(|p| {
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
        roll_loot(&pools)
    } else {
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
            .collect()
    };
    raw.into_iter()
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
/// Vanilla armor piece rows: (item name, equipment slot group, armor points,
/// default `generic.armor_toughness`, default `generic.knockback_resistance`).
/// Points are the piece's baked-in attribute modifier; toughness/resistance
/// only count while the piece carries no explicit `attribute_modifiers`
/// component.
const ARMOR_PIECES: &[(&str, ItemAttrSlotGroup, i32, f64, f64)] = &[
    ("leather_helmet", ItemAttrSlotGroup::Head, 1, 0.0, 0.0),
    ("golden_helmet", ItemAttrSlotGroup::Head, 2, 0.0, 0.0),
    ("chainmail_helmet", ItemAttrSlotGroup::Head, 2, 0.0, 0.0),
    ("iron_helmet", ItemAttrSlotGroup::Head, 2, 0.0, 0.0),
    ("diamond_helmet", ItemAttrSlotGroup::Head, 3, 2.0, 0.0),
    ("netherite_helmet", ItemAttrSlotGroup::Head, 3, 3.0, 0.1),
    ("turtle_helmet", ItemAttrSlotGroup::Head, 2, 0.0, 0.0),
    ("leather_chestplate", ItemAttrSlotGroup::Chest, 3, 0.0, 0.0),
    ("golden_chestplate", ItemAttrSlotGroup::Chest, 5, 0.0, 0.0),
    ("chainmail_chestplate", ItemAttrSlotGroup::Chest, 5, 0.0, 0.0),
    ("iron_chestplate", ItemAttrSlotGroup::Chest, 6, 0.0, 0.0),
    ("diamond_chestplate", ItemAttrSlotGroup::Chest, 8, 2.0, 0.0),
    ("netherite_chestplate", ItemAttrSlotGroup::Chest, 8, 3.0, 0.1),
    ("elytra", ItemAttrSlotGroup::Chest, 0, 0.0, 0.0),
    ("leather_leggings", ItemAttrSlotGroup::Legs, 2, 0.0, 0.0),
    ("golden_leggings", ItemAttrSlotGroup::Legs, 3, 0.0, 0.0),
    ("chainmail_leggings", ItemAttrSlotGroup::Legs, 4, 0.0, 0.0),
    ("iron_leggings", ItemAttrSlotGroup::Legs, 5, 0.0, 0.0),
    ("diamond_leggings", ItemAttrSlotGroup::Legs, 6, 2.0, 0.0),
    ("netherite_leggings", ItemAttrSlotGroup::Legs, 6, 3.0, 0.1),
    ("leather_boots", ItemAttrSlotGroup::Feet, 1, 0.0, 0.0),
    ("golden_boots", ItemAttrSlotGroup::Feet, 1, 0.0, 0.0),
    ("chainmail_boots", ItemAttrSlotGroup::Feet, 1, 0.0, 0.0),
    ("iron_boots", ItemAttrSlotGroup::Feet, 2, 0.0, 0.0),
    ("diamond_boots", ItemAttrSlotGroup::Feet, 3, 2.0, 0.0),
    ("netherite_boots", ItemAttrSlotGroup::Feet, 3, 3.0, 0.1),
];
fn armor_piece_row(item: i64, grp: Option<ItemAttrSlotGroup>) -> Option<&'static (&'static str, ItemAttrSlotGroup, i32, f64, f64)> {
    ARMOR_PIECES.iter().find(|(n, g, ..)| {
        grp.is_none_or(|want| *g == want) && name_to_item(n) == Some(item)
    })
}
// Vanilla default `generic.armor_toughness` baked into armor items as their
// default attribute_modifiers component (diamond +2, netherite +3 per piece).
// The default only counts while the piece carries no explicit component.
fn armor_default_toughness(item: i64) -> f64 {
    armor_piece_row(item, None).map_or(0.0, |r| r.3)
}
fn armor_default_knockback_resistance(item: i64) -> f64 {
    armor_piece_row(item, None).map_or(0.0, |r| r.4)
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
    let armor = f64::from(*lk(&c.armor_pts))
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
    let inv = lk(&c.inv);
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
#[cfg(test)]
fn dmg_after(c: &Client, raw: f32, kind: u8) -> f32 {
    dmg_after_breach(c, raw, kind, 0)
}
// call AFTER the hp write: if hp hit 0 and a totem is held (main or
// offhand), consume it — vanilla sets hp to 1, fires entity_status 35,
// plays the totem animation+sound, grants absorption/regen/fire_resist.
// Damage taken below the world bottom is `fellOutOfWorld` (the void caller in
// the main tick only applies it there), a `BYPASSES_INVULNERABILITY` source
// vanilla's `checkTotemDeathProtection` never sees.

/// `Entity.checkBelowWorld`'s floor: vanilla applies `out_of_world` once an
/// entity's Y drops below `level.getMinBuildHeight() - 64`, so the Overworld
/// floor is -128 and the Nether/End floor is -64 (their `min_y` is 0). A custom
/// `dimension_type` moves it with its own declared height.
fn void_damage_floor(dim: DimId) -> f64 {
    f64::from(dim.min_y()) - 64.0
}

fn totem_check(c: &Client) {
    let hp = *lk(&c.hp);
    let kind = {
        let p = lk(&c.pos);
        let below = p.1 < void_damage_floor(lk(&c.dim));
        if c.generic_kill_pending.load(Ordering::SeqCst) {
            TotemDamageKind::Kill
        } else if below {
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
    *lk(&c.absorb) = 8.0;
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
    let slots = [
        (5i16, ItemAttrSlotGroup::Head),
        (6, ItemAttrSlotGroup::Chest),
        (7, ItemAttrSlotGroup::Legs),
        (8, ItemAttrSlotGroup::Feet),
    ];
    let mut pairs: Vec<(ItemAttrSlotGroup, &Stack)> = Vec::new();
    for (slot, grp) in slots {
        if let Some(st @ (item, _, comps)) = inv.get(&slot) {
            // An explicit attribute_modifiers component replaces the piece's
            // vanilla default, so the table points only count without one.
            if comp_get(comps, COMP_ATTRIBUTE_MODIFIERS).is_none() {
                pts += armor_piece_row(*item, Some(grp)).map_or(0, |r| r.2);
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
    // Nothing to pair: an idle world must not pay even the merger borrow.
    if xl.len() < 2 {
        return Vec::new();
    }
    // `ReusableCellMerger::merge_orbs` reproduces this scan bit-identically —
    // integer cell buckets, ascending-index candidates, `dist^2 < 1.0`, the
    // dimension side table including the cleared-eid quirk — out of one
    // retained scratch instead of a fresh `CellBuckets` per pass.
    XP_DROP_MERGER.with(|m| {
        m.borrow_mut()
            .merge_orbs(
                xl,
                |eid| u64::from(entity_dim_of(eid).raw()),
                clear_entity_dim,
            )
            .to_vec()
    })
}

/// Finite segment geometry. Unlike Valence's inclusive infinite ray, pinned
/// AABB.clip tests entering faces, with epsilon on transverse bounds and open t.
#[derive(Clone, Copy)]
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

/// Adapter for historical segment regression fixtures.
#[cfg(all(test, megu_vanilla_data))]
fn projectile_segment_hits(
    start: [f64; 3],
    end: [f64; 3],
    target: (f64, f64, f64),
    rx: f64,
    y_lo: f64,
    y_hi: f64,
) -> bool {
    let vel = std::array::from_fn(|i| end[i] - start[i]);
    let mut spec = ProjectileKind::Arrow.spec();
    spec.hit_margin = 0.0;
    sweep_projectile_segment(
        start,
        vel,
        spec,
        -1,
        &mut FlightContacts::default(),
        |_| None,
        [(
            1,
            (
                [target.0 - rx, target.1 - y_lo, target.2 - rx],
                [target.0 + rx, target.1 + y_hi, target.2 + rx],
            ),
        )],
    )
    .is_some()
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
/// only `hurt_at`) is adopted into the tick window on the next tick.
const MOB_HURT_WINDOW_GT: u64 = 10;

static MOB_LAST_HURT_AMOUNT: OnceLock<Mutex<HashMap<i32, f32>>> = OnceLock::new();
fn mob_last_hurt_amount() -> &'static Mutex<HashMap<i32, f32>> {
    MOB_LAST_HURT_AMOUNT.get_or_init(|| Mutex::new(HashMap::new()))
}
static MOB_EFFECTS: OnceLock<Mutex<HashMap<i32, Vec<Eff>>>> = OnceLock::new();
fn mob_effects() -> &'static Mutex<HashMap<i32, Vec<Eff>>> {
    MOB_EFFECTS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Hard ceiling on `MOB_LAST_HURT_AMOUNT` rows. Both side tables mirror live
/// mobs, which `MOB_CAP` already bounds, and the per-tick retain drops ids the
/// mob list no longer holds — so this is the defensive backstop for a burst of
/// inserts between two sweeps. Evicting a row only re-opens that mob's hurt
/// window on its next hit.
const MAX_MOB_LAST_HURT_ENTRIES: usize = MAX_ALL_MOB_DESPAWN_ENTRIES;
/// Hard ceiling on `MOB_EFFECTS` rows, for the same reason: an evicted entry
/// re-reads as "no effects" until the next application writes it again.
const MAX_MOB_EFFECTS_ENTRIES: usize = MAX_ALL_MOB_DESPAWN_ENTRIES;

/// Enforce [`MAX_MOB_LAST_HURT_ENTRIES`] after the live-id retain sweep.
/// HashMap iteration order is arbitrary; any excess row is re-seeded on the
/// evicted mob's next hurt.
fn bound_mob_last_hurt_amount(map: &mut HashMap<i32, f32>) {
    while map.len() > MAX_MOB_LAST_HURT_ENTRIES {
        let Some(eid) = map.keys().next().copied() else {
            break;
        };
        map.remove(&eid);
    }
}

/// Enforce [`MAX_MOB_EFFECTS_ENTRIES`] after the live-id retain sweep.
fn bound_mob_effects(map: &mut HashMap<i32, Vec<Eff>>) {
    while map.len() > MAX_MOB_EFFECTS_ENTRIES {
        let Some(eid) = map.keys().next().copied() else {
            break;
        };
        map.remove(&eid);
    }
}
fn mob_resistance_damage_factor(mob: &Mob) -> f64 {
    let amp = lk(mob_effects())
        .get(&mob.eid)
        .into_iter()
        .flatten()
        .filter(|effect| effect.left > 0 && effect.id == 10)
        .map(|effect| i64::from(effect.amp))
        .max();
    amp.map_or(1.0, |amp| {
        ((25.0 - 5.0 * (amp as f64 + 1.0)) / 25.0).max(0.0)
    })
}

/// Mob armor is tracked in the equipment side table rather than `Mob` itself.
/// Resolve the four armor slots through the same item rows and explicit
/// `attribute_modifiers` codec players use.
fn mob_armor_values(mob: &Mob) -> (f64, f64) {
    let equipment = lk(mob_equipment());
    let Some(eq) = equipment.get(&mob.eid) else {
        return (0.0, 0.0);
    };
    let slot_rows = [
        (EquipmentSlot::Feet, ItemAttrSlotGroup::Feet),
        (EquipmentSlot::Legs, ItemAttrSlotGroup::Legs),
        (EquipmentSlot::Chest, ItemAttrSlotGroup::Chest),
        (EquipmentSlot::Head, ItemAttrSlotGroup::Head),
    ];
    let mut owned: Vec<(ItemAttrSlotGroup, Stack)> = Vec::new();
    let mut armor_base = 0.0;
    let mut toughness_base = 0.0;
    for (slot, group) in slot_rows {
        let stack = eq.get(slot).clone();
        if stack.1 == 0 {
            continue;
        }
        if comp_get(&stack.2, COMP_ATTRIBUTE_MODIFIERS).is_none() {
            if let Some(row) = armor_piece_row(stack.0, Some(group)) {
                armor_base += f64::from(row.2);
                toughness_base += row.3;
            }
        }
        owned.push((group, stack));
    }
    let pairs: Vec<(ItemAttrSlotGroup, &Stack)> =
        owned.iter().map(|(group, stack)| (*group, stack)).collect();
    (
        apply_item_attribute_modifiers(armor_base, &pairs, ATTR_ARMOR),
        apply_item_attribute_modifiers(toughness_base, &pairs, ATTR_ARMOR_TOUGHNESS),
    )
}

fn mob_melee_epf(mob: &Mob) -> i64 {
    let equipment = lk(mob_equipment());
    let Some(eq) = equipment.get(&mob.eid) else {
        return 0;
    };
    [
        EquipmentSlot::Feet,
        EquipmentSlot::Legs,
        EquipmentSlot::Chest,
        EquipmentSlot::Head,
    ]
    .into_iter()
    .map(|slot| enchant_level(&eq.get(slot).2, 27))
    .sum::<i64>()
    .min(20)
}

/// `LivingEntity.actuallyHurt` for a player melee hit on a mob: armor (with
/// Breach reducing the armor reduction), generic Protection EPF, and any
/// Resistance rows the mob-effect table carries for the victim.
fn mob_melee_mitigate(mob: &Mob, raw: f32, breach_level: i64) -> f32 {
    let (armor, toughness) = mob_armor_values(mob);
    let f = 2.0 + toughness / 4.0;
    let v = armor - f64::from(raw) / f;
    let lo = armor * 0.2;
    let g = if v < lo { lo } else { v.min(20.0) };
    let effectiveness = (1.0 - 0.15 * breach_level.clamp(0, 4) as f64).max(0.0);
    let armor_reduction = (g / 25.0 * effectiveness).clamp(0.0, 1.0);
    let after_armor = f64::from(raw) * (1.0 - armor_reduction);
    let after_resistance = after_armor * mob_resistance_damage_factor(mob);
    (after_resistance * (1.0 - 0.04 * mob_melee_epf(mob) as f64)) as f32
}

/// Vanilla mob melee invulnerability: inside the ten-game-tick window a new
/// hit lands only for its excess over the stored incoming amount. Outside the
/// window the full amount lands. Returns the differential amount to apply.
fn mob_melee_hurt_gate(mob: &Mob, amount: f32) -> Option<f32> {
    let now_tick = TICK_IDX.load(Ordering::Relaxed);
    let elapsed = lk(&mob.last_hurt_tick).map(|tick| now_tick.saturating_sub(tick));
    let accepted = match elapsed {
        Some(ticks) if ticks < MOB_HURT_WINDOW_GT => {
            let last = lk(mob_last_hurt_amount())
                .get(&mob.eid)
                .copied()
                .unwrap_or(0.0);
            if amount <= last {
                return None;
            }
            amount - last
        }
        _ => amount,
    };
    *lk(&mob.last_hurt_tick) = Some(now_tick);
    *lk(&mob.hurt_at) = Instant::now();
    lk(mob_last_hurt_amount()).insert(mob.eid, amount);
    Some(accepted)
}

fn mob_hurt_window_tick() {
    let now_tick = TICK_IDX.load(Ordering::Relaxed);
    let now = Instant::now();
    let reg = breg();
    for mob in lk(mobs()).iter() {
        // `LayeredCauldronBlock.entityInside`: a burning entity standing in a
        // water or powder-snow cauldron is extinguished; water drops a level
        // (level 1 empties) and powder snow becomes water at the same level.
        if *lk(&mob.fire_ticks) > 0 {
            if let Some(reg) = reg {
                let (mx, my, mz, ..) = *lk(&mob.pos);
                let (bx, by, bz) = (mx.floor() as i32, my.floor() as i32, mz.floor() as i32);
                let st = block_at_dim(mob.dim, bx, by, bz);
                if let Some(state) = cauldron_state_of(reg, st) {
                    let quenched = cauldron_extinguish(state);
                    if quenched != state {
                        if let Some(next) = quenched.to_state(reg) {
                            set_block_dim(mob.dim, bx, by, bz, next);
                            note_block_update_dim(mob.dim, bx, by, bz, next);
                        }
                        *lk(&mob.fire_ticks) = 0;
                    }
                }
            }
        }
        let mut hurt_tick = lk(&mob.last_hurt_tick);
        let mut hurt_at = lk(&mob.hurt_at);
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

fn apply_arrow_punch(mob: &Mob, dx: f64, dz: f64, level: i32) {
    let speed = dx.hypot(dz).max(0.01);
    let distance = 0.3 * f64::from(level);
    let (x, y, z, yaw, pitch) = {
        let mut pos = lk(&mob.pos);
        pos.0 += dx / speed * distance;
        pos.2 += dz / speed * distance;
        *pos
    };
    broadcast_seen(mob.eid, |_| (0x70, entity_teleport(mob.eid, x, y, z, yaw, pitch, true)));
}

/// (victim eid, dmg, arrow x, z, owner eid, kind, custom-named stack)
type ArrowHit = (DimId, i32, f32, f64, f64, i32, u8, Option<String>);

/// Trident pre-flight gate for `arrow_tick`: Loyalty return flight and the
/// first-observation Riptide resolve. `true` means the projectile was consumed
/// this tick (the caller's `continue`).
fn arrow_trident_gate(
    a: &mut Arrow,
    eid: i32,
    dead: &mut Vec<i32>,
    returning: &mut HashSet<i32>,
    seen_tridents: &mut HashSet<i32>,
) -> bool {
    // Loyalty return flight (`ThrownTrident.tick` once `dealtDamage` is set):
    // `delta = delta * 0.95 + dir(owner_eye).normalize() * 0.05 * loyalty`,
    // the trident's own Y follows `dir.y * 0.015 * loyalty`, and the return
    // whoosh plays once on the first return tick. No dimension floor gates it,
    // so a trident below `min_y` still comes home.
    if returning.contains(&eid) {
        let owner_pos = lk(clients())
            .iter()
            .find(|c| c.eid == a.owner && *lk(&c.dim) == entity_dim_of(eid))
            .map(|c| {
                let p = lk(&c.pos);
                let eye = f64::from(player_eye_height(*lk(&c.sneaking), *lk(&c.fall_fly)));
                (p.0, p.1 + eye, p.2)
            });
        // `ThrownTrident.isAcceptibleReturnOwner`, false path: the trident is
        // spawned as an item entity where it hangs, in its own dimension
        // (`arrow_land_tridents` does the drop), never deleted outright.
        let Some((ox, oy, oz)) = owner_pos else {
            returning.remove(&eid);
            seen_tridents.remove(&eid);
            dead.push(eid);
            return true;
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
            seen_tridents.remove(&eid);
            dead.push(eid);
            return true;
        }
        let imp = eval.loyalty_return.unwrap_or([0.0; 3]);
        a.dx = a.dx * 0.95 + imp[0];
        a.dy = a.dy * 0.95 + imp[1];
        a.dz = a.dz * 0.95 + imp[2];
        // `setPosRaw(getX(), getY() + d.y * 0.015 * loyalty, getZ())`.
        a.y += (d[1] / n) * 0.015 * a.loy as f64;
        a.x += a.dx;
        a.y += a.dy;
        a.z += a.dz;
        let (ax, ay, az) = (a.x, a.y, a.z);
        broadcast_seen(eid, |_c| {
            (0x70, entity_teleport(eid, ax, ay, az, 0.0, 0.0, true))
        });
        return true;
    }
    // Riptide resolves the first tick the projectile is observed:
    // vanilla `releaseUsing` never spawns one — the thrower is
    // propelled along the look vector (wet-only) and keeps the item.
    if seen_tridents.insert(eid) {
        let riptide = enchant_level(&a.ret_comps, 31);
        if (1..=3).contains(&riptide) {
            let owner = lk(clients())
                .iter()
                .find(|c| c.eid == a.owner && *lk(&c.dim) == entity_dim_of(eid))
                .cloned();
            if let Some(oc) = owner {
                let (x, y, z, yaw, pitch, on_ground) = *lk(&oc.pos);
                let (yr, pr) = ((yaw as f64).to_radians(), (pitch as f64).to_radians());
                let eval = evaluate_trident_special(&TridentSpecialCtx {
                    riptide,
                    too_damaged: trident_too_damaged(&a.ret_comps),
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
                    lk(riptide_spin()).insert(
                        oc.eid,
                        TICK_IDX.load(Ordering::Relaxed) + server_ticks(u64::from(launch.spin_ticks)),
                    );
                    broadcast_spin_flags(&oc, true);
                    if launch.ground_boost > 0.0 {
                        let mut p = lk(&oc.pos);
                        p.1 += launch.ground_boost;
                    }
                    // The release sound is the enchantment's `trident_sound`
                    // entry for the highest level: riptide_1..3.
                    let snd = sound_pkt_named_ex(
                        &format!("item.trident.riptide_{riptide}"),
                        SOUND_SOURCE_PLAYERS,
                        x,
                        y,
                        z,
                        1.0,
                        1.0,
                    );
                    broadcast_seen(oc.eid, |_c| (0x68, snd.clone()));
                }
                // Refused launches also never leave the hand.
                trident_restore(&oc, a.ret_item, &a.ret_comps);
                a.ret_item = 0;
                dead.push(eid);
                return true;
            }
        }
    }
    false
}

/// Stuck-arrow TTL countdown and the survival pickup pass
/// (`AbstractArrow.tryPickup`). Always consumes the tick.
fn arrow_stuck_step(a: &mut Arrow, eid: i32, adim: DimId, dead: &mut Vec<i32>) {
    a.ttl -= 1;
    if a.ttl <= 0 {
        dead.push(eid);
        return;
    }
    // `intangible_projectile` arrows (pillager shots) are never
    // picked up: vanilla `AbstractArrow.tryPickup` refuses them.
    if comp_get(&a.ret_comps, COMP_INTANGIBLE_PROJECTILE).is_some() {
        return;
    }
    // Vanilla: a stuck arrow is picked up on contact (survival only).
    let (ax, ay, az) = (a.x, a.y, a.z);
    let grab = {
        lk(clients())
            .iter()
            .filter(|c| *lk(&c.gm) != 1)
            .find(|c| {
                if *lk(&c.dim) != adim {
                    return false;
                }
                let p = lk(&c.pos);
                let (dx, dy, dz) = (p.0 - ax, (p.1 + 0.9) - ay, p.2 - az);
                dx * dx + dy * dy + dz * dz < 1.3
            })
            .cloned()
    };
    if let Some(c) = grab {
        let slot: i16 = {
            let mut inv = lk(&c.inv);
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
            slot
        };
        let st = lk(&c.inv)
            .get(&slot)
            .cloned()
            .unwrap_or((802, 1, Vec::new()));
        let mut p = vec![0u8];
        write_varint(&mut p, i64::from(inv_state_next(&c)));
        p.extend_from_slice(&slot.to_be_bytes());
        p.extend_from_slice(&slot_wire(&st));
        if let Ok(mut w) = c.sock.lock() {
            let _ = send_packet(&mut w, 0x15, p);
        }
        dead.push(eid);
    }
}

/// Snapshot target geometry once per projectile family, without retaining
/// world or entity locks across damage callbacks.
fn projectile_targets(players: &[Arc<Client>]) -> Vec<(DimId, i32, ProjectileBounds)> {
    let mut targets = Vec::new();
    for m in lk(mobs()).iter() {
        if *lk(&m.hp) <= 0.0 {
            continue;
        }
        let (x, y, z, ..) = *lk(&m.pos);
        let (width, height) = entity_dimensions_1211(m.etype);
        let r = f64::from(width) * 0.5;
        targets.push((
            m.dim,
            m.eid,
            ([x - r, y, z - r], [x + r, y + f64::from(height), z + r]),
        ));
    }
    for c in players {
        if matches!(*lk(&c.gm), 1 | 3) || *lk(&c.hp) <= 0.0 {
            continue;
        }
        let dim = *lk(&c.dim);
        let (x, y, z, ..) = *lk(&c.pos);
        let sneaking = *lk(&c.sneaking);
        let flying = *lk(&c.fall_fly);
        let swimming =
            *lk(&c.sprinting) && aabb_in_water_dim(dim, player_bounding_box(x, y, z, false, false));
        let pose = megu_core::pose_ray::movement_pose(sneaking, swimming, flying, |pose| {
            player_pose_fits(dim, x, y, z, f64::from(pose.dimensions().height))
        });
        targets.push((dim, c.eid, pose.dimensions().bounds([x, y, z])));
    }
    targets
}

// Projectile stores have literal constructors and can be drained on disconnect
// or reload. Retire flight history for IDs absent from the preceding pass.
#[derive(Default)]
struct ProjectileFlights(HashMap<i32, (FlightContacts, bool)>);

impl ProjectileFlights {
    fn begin(store: &OnceLock<Mutex<Self>>) -> std::sync::MutexGuard<'_, Self> {
        let mut flights = lk(store.get_or_init(Default::default));
        flights.0.retain(|_, (_, present)| std::mem::take(present));
        flights
    }

    fn track(&mut self, eid: i32) -> &mut FlightContacts {
        let (flight, present) = self.0.entry(eid).or_default();
        *present = true;
        flight
    }
}

struct ProjectilePath<'a> {
    kind: ProjectileKind,
    dim: DimId,
    owner: i32,
    targets: &'a [(DimId, i32, ProjectileBounds)],
}

impl ProjectilePath<'_> {
    fn hit(
        &self,
        pos: [f64; 3],
        vel: [f64; 3],
        flight: &mut FlightContacts,
    ) -> Option<ProjectileImpact> {
        let reg = breg();
        sweep_projectile_segment(
            pos,
            vel,
            self.kind.spec(),
            self.owner,
            flight,
            |[x, y, z]| {
                let state = block_at_dim(self.dim, x, y, z);
                // The gateway is a special impact for pearls despite its empty shape.
                let gateway =
                    self.kind == ProjectileKind::EnderPearl && is_end_gateway_state(state);
                let full_cube = gateway || (reg.is_none() && !mob_passable(state));
                reg.into_iter()
                    .flat_map(move |r| r.collision_boxes(state))
                    .chain(full_cube.then_some(([0.; 3], [1.; 3])))
            },
            self.targets
                .iter()
                .filter(|(dim, ..)| *dim == self.dim)
                .map(|&(_, id, bounds)| (id, bounds)),
        )
    }

    fn step(&self, pos: [f64; 3], vel: [f64; 3]) -> ([f64; 3], [f64; 3]) {
        let [x, y, z] = pos.map(|p| p.floor() as i32);
        let wet = breg().is_some_and(|r| r.has_water(block_at_dim(self.dim, x, y, z)));
        step_projectile_kinematics(pos, vel, wet, self.kind.spec())
    }
}

/// Channeling gate for a trident mob hit: thunder and open sky above the
/// struck position (punch carries the enchant level for kind==1).
fn arrow_channeling_hit(a: &Arrow, mx: f64, my: f64, mz: f64, adim: DimId) -> bool {
    a.kind == 1
        && a.punch > 0
        && evaluate_trident_special(&TridentSpecialCtx {
            channeling: i64::from(a.punch),
            thundering: THUNDERING.load(Ordering::Relaxed) > 0,
            open_sky: THUNDERING.load(Ordering::Relaxed) > 0
                && can_see_sky(
                    adim,
                    mx.floor() as i32,
                    my.floor() as i32,
                    mz.floor() as i32,
                ),
            ..Default::default()
        })
        .channeling_lightning
}

/// The lightning a channeling trident calls on a mob hit: 5 damage, a creeper
/// fuse and the thunder/bolt packets.
fn arrow_channeling_effect(m: &Mob, mx: f64, my: f64, mz: f64) {
    *lk(&m.hp) -= 5.0;
    *lk(&m.fuse) = server_ticks(CREEPER_FUSE_GT) as i32; // ignite ~5s
    let be = NEXT_EID.fetch_add(1, Ordering::SeqCst);
    lk(bolts()).push((be, server_ticks(BOLT_LIFE_GT) as i32));
    broadcast(-1, |_c| (0x01, spawn_typed_pkt(be, 64, mx, my, mz)));
    let ts = sound_pkt_named("entity.lightning_bolt.thunder", mx, my, mz);
    broadcast(-1, |_c| (0x68, ts.clone()));
}

/// Apply mob damage and its effects. The flight loop resolves piercing and Loyalty.
fn arrow_strike_mob(
    a: &Arrow,
    m: &Mob,
    dead: &mut Vec<i32>,
    mob_dead: &mut Vec<(i32, i64, f64, f64, f64)>,
) {
    let (mx, my, mz, ..) = *lk(&m.pos);
    let kind = match a.kind {
        0 => BossHit::Arrow,
        1 => BossHit::Trident,
        _ => BossHit::Other,
    };
    if !live_mob_hit_allowed(m, a.owner, kind) {
        dead.push(a.eid);
        return;
    }
    // Mod-suite `EntityDamagePre` for the struck mob: a
    // cancelled hit is consumed like any other impact
    // but deals nothing.
    let raw = if a.kind == 1 {
        // `ThrownTrident.onHitEntity`: 8.0 base plus the trident's own
        // type-gated enchantment damage against *this* victim.
        thrown_trident_hit_damage(a, Some(m.etype))
    } else {
        a.dmg
    };
    let Some(dmg) = live_mod_entity_damage_pre(raw) else {
        return;
    };
    *lk(&m.hurt_at) = Instant::now();
    *lk(&m.last_hurt_tick) = Some(TICK_IDX.load(Ordering::Relaxed));
    let dh = {
        let mut h = lk(&m.hp);
        *h -= dmg;
        *h
    };
    if a.fire > 0 && !mob_fire_immune(m.etype) {
        *lk(&m.fire_ticks) = a.fire;
        ignite_mob(m.eid, a.fire.max(0) as u32);
    }
    if a.punch > 0 && a.kind == 0 {
        apply_arrow_punch(m, a.dx, a.dz, a.punch);
    }
    broadcast_seen(m.eid, |_c| (0x24, hurt_anim(m.eid)));
    if arrow_channeling_hit(a, mx, my, mz, m.dim) {
        arrow_channeling_effect(m, mx, my, mz);
    }
    if dh <= 0.0 {
        mob_dead.push((m.eid, m.etype, mx, my, mz));
        let on = lk(clients())
            .iter()
            .find(|c| c.eid == a.owner)
            .map(|c| c.name.clone());
        if let Some(on) = on {
            adv_event(&on, "player_killed_entity", Some(m.etype));
        }
    }
}

/// Thrown tridents land as pickupable drops; loyalty returns at the owner's feet.
fn arrow_land_tridents(
    ar: &mut Vec<Arrow>,
    dead: &[i32],
    returning: &mut HashSet<i32>,
    seen_tridents: &mut HashSet<i32>,
) {
    let landed: Vec<_> = ar
        .iter()
        .filter(|a| dead.contains(&a.eid) && a.ret_item > 0)
        .map(|a| {
            (
                entity_dim_of(a.eid),
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
    for (dim, ow, lx, ly, lz, loy, ri, rc) in landed {
        let (tx, ty, tz) = if loy > 0 {
            lk(clients())
                .iter()
                .find(|c| c.eid == ow && *lk(&c.dim) == dim)
                .map(|c| {
                    let p = lk(&c.pos);
                    (p.0, p.1, p.2)
                })
                .unwrap_or((lx, ly, lz))
        } else {
            (lx, ly, lz)
        };
        drop_spawn_in_dim(
            dim,
            DropSpawn {
                x: tx,
                y: ty,
                z: tz,
                item: ri,
                cnt: TRIDENT_STACK_LIMIT as u8,
                comps: rc,
                age: 0,
                target: None,
                thrower: None,
            },
        );
    }
}

/// One recorded projectile hit on a player victim: shield gate,
/// invulnerability, absorption/armor/totem and the hurt+health packets,
/// then the vanilla death line when the hit killed.
fn arrow_apply_hit((dim, veid, admg, ax, az, owner_eid, proj_kind, proj_weapon): ArrowHit) {
    let cls: Vec<Arc<Client>> = client_snapshot();
    let Some(c) = cls.iter().find(|c| c.eid == veid && *lk(&c.dim) == dim) else {
        return;
    };
    if *lk(&c.gm) == 1 {
        return;
    }
    // Mod-suite `EntityDamagePre` for the struck player: a cancelled
    // hit skips this victim entirely.
    let Some(admg) = live_mod_entity_damage_pre(admg) else {
        return;
    };
    // the evaluator's raise clock already tracks `blocking` — a lowered
    // shield reports 0 raised ticks, below the warmup gate
    let eval = evaluate_shield_block(&shield_hit_ctx(c, [ax, 0.0, az], admg));
    let blocked_ar = eval.blocked && *lk(&c.blocking);
    // a rejected hit must not cost shield durability (vanilla returns before that)
    let Some((ad, _)) = damage_after_invulnerability(c, if blocked_ar { 0.0 } else { admg })
    else {
        return;
    };
    if blocked_ar {
        damage_shield_attempts(c, eval.durability_attempts);
    }
    let damage_type = if proj_kind == 1 { "trident" } else { "arrow" };
    let source = if owner_eid >= 0 {
        DamageSource::new(damage_type).with_cause(owner_eid)
    } else {
        DamageSource::new(damage_type)
    };
    let hh = player_hurt_apply(c, ad, &source, (ad > 0.0).then_some(ad));
    let vname = c.name.clone();
    // Legacy skeleton shots carry a negative owner sentinel.
    let attacker = if owner_eid >= 0 {
        cls.iter().find(|p| p.eid == owner_eid).map(|p| p.name.clone())
            .or_else(|| damage_attacker_name(&format!("m:{owner_eid}"), &cls))
    } else {
        Some("Skeleton".to_owned())
    };
    broadcast_seen_ex(veid, veid, |_c| (0x24, hurt_anim(veid)));
    if hh <= 0.0 && GR_SHOW_DEATH_MESSAGES.load(Ordering::Relaxed) != 0 {
        let line = system_chat(&format_vanilla_death_message(
            &vname,
            damage_type,
            attacker.as_deref(),
            proj_weapon.as_deref(),
        ));
        broadcast(-1, |_c| (0x6c, line.clone()));
    }
}

fn arrow_tick() {
    mob_hurt_window_tick();
    combat2_state_tick();
    let mut dead: Vec<i32> = vec![];
    let mut hits: Vec<ArrowHit> = vec![];
    let mut mob_dead: Vec<(i32, i64, f64, f64, f64)> = vec![];
    {
        let mut ar = lk(arrows());
        static FLIGHTS: OnceLock<Mutex<ProjectileFlights>> = OnceLock::new();
        let mut flights = ProjectileFlights::begin(&FLIGHTS);
        if ar.is_empty() {
            return;
        }
        let targets = projectile_targets(&client_snapshot());
        let region = region_guard();
        let mut returning = lk(trident_returning());
        let mut seen_tridents = lk(trident_seen());
        for a in ar.iter_mut() {
            let eid = a.eid;
            // The projectile's own dimension: block probes, target filters and
            // the channeling sky check all resolve against `adim`'s world.
            let adim = entity_dim_of(eid);
            let flight = flights.track(eid);
            if !region.is_hot(a.x as i32, a.z as i32) {
                continue;
            }
            if a.kind == 1
                && arrow_trident_gate(a, eid, &mut dead, &mut returning, &mut seen_tridents)
            {
                continue;
            }
            if a.stuck {
                arrow_stuck_step(a, eid, adim, &mut dead);
                continue;
            }
            let path = ProjectilePath {
                kind: if a.kind == 1 {
                    ProjectileKind::Trident
                } else {
                    ProjectileKind::Arrow
                },
                dim: adim,
                owner: a.owner,
                targets: &targets,
            };
            let pos = [a.x, a.y, a.z];
            let vel = [a.dx, a.dy, a.dz];
            let (end, next_vel) = path.step(pos, vel);
            if a.last_hit >= 0 && !flight.pierced.contains(&a.last_hit) {
                flight.pierced.push(a.last_hit);
            }
            loop {
                let Some(hit) = path.hit(pos, vel, flight) else {
                    [a.x, a.y, a.z] = end;
                    [a.dx, a.dy, a.dz] = next_vel;
                    break;
                };
                [a.x, a.y, a.z] = hit.point;
                let ProjectileContact::Entity(id) = hit.contact else {
                    if a.kind == 1 {
                        if a.loy > 0 {
                            trident_return_whoosh(a);
                            returning.insert(eid);
                        } else {
                            dead.push(eid);
                        }
                    } else {
                        a.stuck = true;
                        a.ttl = server_ticks(ARROW_TTL_GT) as i32;
                        [a.dx, a.dy, a.dz] = [0.0; 3];
                    }
                    break;
                };
                let mob = lk(mobs())
                    .iter()
                    .find(|m| m.eid == id && m.dim == adim)
                    .cloned();
                if let Some(m) = mob {
                    arrow_strike_mob(a, &m, &mut dead, &mut mob_dead);
                } else {
                    hits.push((
                        adim,
                        id,
                        if a.kind == 1 {
                            thrown_trident_hit_damage(a, None)
                        } else {
                            a.dmg
                        },
                        a.x,
                        a.z,
                        a.owner,
                        a.kind,
                        renamed_custom_name(&a.ret_comps),
                    ));
                }
                if dead.contains(&eid) {
                    break;
                }
                if a.pierce <= 0 {
                    if a.kind == 1 && a.loy > 0 {
                        trident_return_whoosh(a);
                        returning.insert(eid);
                    } else {
                        dead.push(eid);
                    }
                    break;
                }
                a.pierce -= 1;
                a.last_hit = id;
                flight.pierced.push(id);
            }
            flight.age_ticks = flight.age_ticks.saturating_add(1);
            broadcast_seen(eid, |_c| {
                (0x70, entity_teleport(eid, a.x, a.y, a.z, 0.0, 0.0, true))
            });
        }
        arrow_land_tridents(&mut ar, &dead, &mut returning, &mut seen_tridents);
    }
    for (deid, etype, mx, my, mz) in mob_dead {
        kill_mob(deid, etype, mx, my, mz);
    }
    for eid in &dead {
        broadcast_seen_destroy(*eid);
        clear_entity_dim(*eid);
    }
    for hit in hits {
        arrow_apply_hit(hit);
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

fn blast_cell_at(dim: DimId, x: i32, y: i32, z: i32) -> BlastSample {
    let st = block_at_dim(dim, x, y, z);
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
fn blast_compute(dim: DimId, origin: [f64; 3], power: f32, seed: u64) -> Option<BlastOut> {
    let source = |x: i32, y: i32, z: i32| {
        let s = blast_cell_at(dim, x, y, z);
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
    lk(blast_engine())
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
fn blast_compute(dim: DimId, origin: [f64; 3], power: f32, seed: u64) -> Option<BlastOut> {
    let mut engine = lk(blast_engine());
    if !(power.is_finite() && power > 0.0) || !engine.prepare(dim, origin, power) {
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
    fn prepare(&mut self, dim: DimId, origin: [f64; 3], power: f32) -> bool {
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
                        dim,
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

/// One queued mega-blast: its plan plus the dimension, origin and power the
/// per-slice removals and explosion packets carry.
struct MegaBlastJob {
    dim: DimId,
    center: [f64; 3],
    power: f32,
    slices: MegaBlastSlices,
}

impl MegaBlastJob {
    fn new(dim: DimId, center: [f64; 3], power: f32, out: BlastOut) -> Self {
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
            dim,
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
fn apply_blast_slice(
    dim: DimId,
    base: [i32; 3],
    removals: &[BlastRemoval],
) -> Vec<(i8, i8, i8)> {
    let mut offs: Vec<(i8, i8, i8)> = Vec::with_capacity(removals.len());
    let mut counts: HashMap<i64, u64> = HashMap::new();
    let mut first_pos: HashMap<i64, [i32; 3]> = HashMap::new();
    let mut primed_new: Vec<[i32; 3]> = Vec::new();
    let tile_drops = GR_DO_TILE_DROPS.load(Ordering::Relaxed) != 0;
    for r in removals {
        let [x, y, z] = r.pos;
        let st = block_at_dim(dim, x, y, z);
        if st == 0 {
            continue;
        }
        // Persistent claims: blocks inside a claimed chunk are immune to
        // every blast source (TNT, creepers, end crystals, respawn
        // anchors). Skipped cells stay in the world, never drop, and never
        // chain-prime TNT — matching how blast-immune blocks are simply
        // absent from the removal set.
        if !can_explosion_damage_block(dim, base[0], base[2], x, z, None) {
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
        set_block_dim(dim, x, y, z, 0);
        offs.push((
            (x - base[0]).clamp(-128, 127) as i8,
            (y - base[1]).clamp(-128, 127) as i8,
            (z - base[2]).clamp(-128, 127) as i8,
        ));
    }
    for [tx, ty, tz] in primed_new {
        let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
        let jitter = ((tx.wrapping_mul(31) ^ tz.wrapping_mul(17) ^ ty) & 0x3ff) as u64;
        lk(primed()).push(Primed {
            eid,
            x: tx as f64 + 0.5,
            y: ty as f64,
            z: tz as f64 + 0.5,
            left: chain_fuse_left(jitter) as i32,
        });
        // A chained TNT stays in the dimension its block was destroyed in.
        set_entity_dim(eid, dim);
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

// ---------- vanilla explosion visibility and damage ----------
fn collision_voxel_ray_hit(
    dim: DimId,
    bx: i32,
    by: i32,
    bz: i32,
    start: [f64; 3],
    end: [f64; 3],
) -> bool {
    let state = block_at_dim(dim, bx, by, bz);
    if state == 0 {
        return false;
    }
    if let Some(reg) = breg() {
        for (min, max) in reg.collision_boxes(state) {
            let min = [
                min[0] + f64::from(bx),
                min[1] + f64::from(by),
                min[2] + f64::from(bz),
            ];
            let max = [
                max[0] + f64::from(bx),
                max[1] + f64::from(by),
                max[2] + f64::from(bz),
            ];
            let shape = Aabb::new(min, max);
            if shape.contains(start) || shape.clip(start, end).is_some() {
                return true;
            }
        }
        return false;
    }
    if !mob_passable(state) {
        let shape = Aabb::new(
            [f64::from(bx), f64::from(by), f64::from(bz)],
            [
                f64::from(bx) + 1.0,
                f64::from(by) + 1.0,
                f64::from(bz) + 1.0,
            ],
        );
        return shape.contains(start) || shape.clip(start, end).is_some();
    }
    false
}

/// Whether a ray hits block collision geometry (`ClipContext.Block.COLLIDER`).
/// The traversal is the same voxel DDA shape vanilla's block clip uses; each
/// entered voxel contributes every collision shape of its block state.
fn collider_ray_hit(dim: DimId, start: [f64; 3], end: [f64; 3]) -> bool {
    let delta = [
        end[0] - start[0],
        end[1] - start[1],
        end[2] - start[2],
    ];
    if delta.iter().all(|d| d.abs() < 1.0e-8) {
        let voxel = start.map(|v| v.floor() as i32);
        return collision_voxel_ray_hit(dim, voxel[0], voxel[1], voxel[2], start, end);
    }
    let mut voxel = start.map(|v| v.floor() as i32);
    let mut step = [0i32; 3];
    let mut t_max = [f64::INFINITY; 3];
    let mut t_delta = [f64::INFINITY; 3];
    for axis in 0..3 {
        if delta[axis] > 1.0e-12 {
            step[axis] = 1;
            let next = f64::from(voxel[axis]) + 1.0;
            t_max[axis] = (next - start[axis]) / delta[axis];
            t_delta[axis] = 1.0 / delta[axis];
        } else if delta[axis] < -1.0e-12 {
            step[axis] = -1;
            t_max[axis] = (f64::from(voxel[axis]) - start[axis]) / delta[axis];
            t_delta[axis] = -1.0 / delta[axis];
        }
    }
    for _ in 0..1024 {
        if collision_voxel_ray_hit(dim, voxel[0], voxel[1], voxel[2], start, end) {
            return true;
        }
        let axis = if t_max[0] <= t_max[1] && t_max[0] <= t_max[2] {
            0
        } else if t_max[1] <= t_max[2] {
            1
        } else {
            2
        };
        if !t_max[axis].is_finite() || t_max[axis] > 1.0 {
            return false;
        }
        voxel[axis] += step[axis];
        t_max[axis] += t_delta[axis];
    }
    true
}

/// Vanilla `Explosion.getSeenPercent`: cast rays from a grid over the target
/// AABB towards the explosion centre and return the unobstructed fraction.
/// X/Z samples carry the vanilla half-remainder inset; Y does not.
fn seen_percent(center: [f64; 3], aabb: Aabb, dim: DimId) -> f32 {
    let [min, max] = [aabb.min, aabb.max];
    let step = [
        1.0 / ((max[0] - min[0]) * 2.0 + 1.0),
        1.0 / ((max[1] - min[1]) * 2.0 + 1.0),
        1.0 / ((max[2] - min[2]) * 2.0 + 1.0),
    ];
    let inset = [
        (1.0 - (1.0 / step[0]).floor() * step[0]) / 2.0,
        0.0,
        (1.0 - (1.0 / step[2]).floor() * step[2]) / 2.0,
    ];
    if step.iter().any(|v| !v.is_finite() || *v <= 0.0) {
        return 0.0;
    }
    let mut visible = 0i32;
    let mut total = 0i32;
    let mut x_frac = 0.0;
    while x_frac <= 1.0 {
        let mut y_frac = 0.0;
        while y_frac <= 1.0 {
            let mut z_frac = 0.0;
            while z_frac <= 1.0 {
                let sample = [
                    min[0] + (max[0] - min[0]) * x_frac + inset[0],
                    min[1] + (max[1] - min[1]) * y_frac,
                    min[2] + (max[2] - min[2]) * z_frac + inset[2],
                ];
                total += 1;
                if !collider_ray_hit(dim, sample, center) {
                    visible += 1;
                }
                z_frac += step[2];
            }
            y_frac += step[1];
        }
        x_frac += step[0];
    }
    if total == 0 {
        0.0
    } else {
        visible as f32 / total as f32
    }
}

fn player_explosion_aabb(x: f64, y: f64, z: f64) -> Aabb {
    Aabb::new([x - 0.3, y, z - 0.3], [x + 0.3, y + 1.8, z + 0.3])
}

fn explosion_knockback_strength(normalized_distance: f64, exposure: f32, resistance: f64) -> f64 {
    (1.0 - normalized_distance) * f64::from(exposure) * (1.0 - resistance.clamp(0.0, 1.0))
}

fn explosion_damage_value(power: f64, distance: f64, exposure: f32) -> f32 {
    if exposure <= 0.0 {
        return 0.0;
    }
    let double_radius = power * 2.0;
    let normalized = distance / double_radius;
    if normalized.is_nan() || normalized > 1.0 {
        return 0.0;
    }
    let impact = (1.0 - normalized) * f64::from(exposure);
    (((impact * impact + impact) / 2.0) * 7.0 * double_radius + 1.0) as f32
}

/// Drain one queued mega-blast slice per tick; `explode_at` also invokes this
/// once after enqueueing so the first ring still lands immediately.
fn mega_blast_tick_drain() {
    let (dim, center, power, removals) = {
        let mut q = lk(mega_blast_queue());
        let Some(job) = q.front_mut() else {
            return;
        };
        let Some(removals) = job.next_slice() else {
            q.pop_front();
            return;
        };
        let (dim, center, power) = (job.dim, job.center, job.power);
        if job.is_complete() {
            q.pop_front();
        }
        (dim, center, power, removals)
    };
    if removals.is_empty() {
        return;
    }
    let base = [
        center[0].floor() as i32,
        center[1].floor() as i32,
        center[2].floor() as i32,
    ];
    let offs = apply_blast_slice(dim, base, &removals);
    broadcast_near(dim, center[0], center[1], center[2], -1, |_c| {
        (
            0x20,
            explosion_pkt_with_power_and_interaction(
                center[0],
                center[1],
                center[2],
                power,
                &offs,
                EXPLOSION_BLOCK_INTERACTION_DESTROY_WITH_DECAY,
            ),
        )
    });
}

/// `explode_at` for callers that predate per-entity dimensions: the blast
/// lands in the Overworld, the dimension they always assumed.
#[cfg(all(test, megu_vanilla_data))]
fn explode_at(ex: f64, ey: f64, ez: f64, power: f64) {
    explode_at_dim(DimId::Overworld, ex, ey, ez, power);
}

/// One explosion in `dim`: the raycast reads that dimension's blocks, the
/// removals write that dimension's overlay, chained TNT keeps the dimension,
/// and only that dimension's players take blast damage.
fn explode_at_dim(dim: DimId, ex: f64, ey: f64, ez: f64, power: f64) {
    explode_at_dim_griefing(dim, ex, ey, ez, power, true);
}

fn explode_at_dim_griefing(dim: DimId, ex: f64, ey: f64, ez: f64, power: f64, griefing: bool) {
    // Fail closed: non-finite or non-positive input mutates nothing — a
    // NaN/infinite, zero or negative power is a no-op, not a hole in the world.
    if !(ex.is_finite() && ey.is_finite() && ez.is_finite() && power.is_finite() && power > 0.0)
    {
        return;
    }
    let power = power.min(MEGA_BLAST_MAX_POWER);
    let origin = [ex, ey, ez];
    if griefing {
        let Some(out) = blast_compute(dim, origin, power as f32, world_rng::next_u64()) else {
            // A blast volume beyond the kernel's bounded cache fails closed:
            // nothing is destroyed and nothing is queued.
            return;
        };
        if power > MEGA_BLAST_IMMEDIATE_POWER || out.removal_count() > MEGA_BLAST_MAX_PER_TICK {
            // Queue the concentric-ring plan, then take this tick's slice — the
            // first ring still lands immediately when the queue was empty.
            let job = MegaBlastJob::new(dim, origin, power as f32, out);
            {
                let mut q = lk(mega_blast_queue());
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
            let offs = apply_blast_slice(dim, base, &out.into_removals());
            broadcast_near(dim, ex, ey, ez, -1, |_c| {
                (
                    0x20,
                    explosion_pkt_with_power_and_interaction(
                        ex,
                        ey,
                        ez,
                        power as f32,
                        &offs,
                        EXPLOSION_BLOCK_INTERACTION_DESTROY_WITH_DECAY,
                    ),
                )
            });
        }
    } else {
        // Explosions owned by mobs still detonate under `mobGriefing=false`;
        // only the block-removal side is suppressed.
        broadcast_near(dim, ex, ey, ez, -1, |_c| {
            (
                0x20,
                explosion_pkt_with_power_and_interaction(
                    ex,
                    ey,
                    ez,
                    power as f32,
                    &[],
                    EXPLOSION_BLOCK_INTERACTION_KEEP,
                ),
            )
        });
    }
    ac_note_explosion(dim, [ex, ey, ez], power);
    explosion_apply_players(dim, origin, power);
}

fn explosion_apply_players(dim: DimId, origin: [f64; 3], power: f64) {
    let [ex, ey, ez] = origin;
    let double_radius = power * 2.0;
    for cl in client_snapshot() {
        if *lk(&cl.dim) != dim {
            continue;
        }
        let gm = *lk(&cl.gm);
        if gm == 3 {
            continue;
        }
        let pos = {
            let p = lk(&cl.pos);
            *p
        };
        let (px, py, pz, ..) = pos;
        let distance = ((px - ex).powi(2) + (py - ey).powi(2) + (pz - ez).powi(2)).sqrt();
        let normalized = distance / double_radius;
        if normalized.is_nan() || normalized > 1.0 {
            continue;
        }
        let exposure = seen_percent(origin, player_explosion_aabb(px, py, pz), dim);
        if exposure <= 0.0 {
            continue;
        }
        if gm != 1 {
            let raw = diff_expl_dmg(explosion_damage_value(power, distance, exposure));
            // Mod-suite `EntityDamagePre`: a cancelled/rewritten hit skips
            // or reshapes this player's share of the blast. Knockback is a
            // separate explosion impulse and is not gated by hurt immunity.
            if let Some(raw) = live_mod_entity_damage_pre(raw) {
                if let Some((accepted, _)) = damage_after_invulnerability(&cl, raw) {
                    let source = DamageSource::new("explosion");
                    player_hurt_apply(&cl, accepted, &source, Some(raw));
                }
            }
        }
        // Vanilla explosion knockback direction uses the eye-position delta,
        // while its strength is the impact fraction reduced by
        // `generic.explosion_knockback_resistance` (including Blast Protection).
        let eye_y = client_eye_y(&cl, dim, px, py, pz);
        let (mut dx, mut dy, mut dz) = (px - ex, eye_y - ey, pz - ez);
        let len = (dx * dx + dy * dy + dz * dz).sqrt();
        if len > 1.0e-7 {
            dx /= len;
            dy /= len;
            dz /= len;
        }
        let strength = explosion_knockback_strength(
            normalized,
            exposure,
            explosion_knockback_resist_of(&cl),
        );
        let packet = velocity_pkt(
            cl.eid,
            (dx * strength * 8000.0) as i16,
            (dy * strength * 8000.0) as i16,
            (dz * strength * 8000.0) as i16,
        );
        if let Ok(mut sock) = cl.sock.lock() {
            let _ = send_packet(&mut sock, 0x5a, packet);
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
        let mut pl = lk(primed());
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
        let pdim = entity_dim_of(p.eid);
        clear_entity_dim(p.eid);
        explode_at_dim(pdim, p.x, p.y, p.z, 4.0);
    }
}

fn live_mob_hit_allowed(mob: &Mob, source: i32, hit: BossHit) -> bool {
    record_mob_loot_attacker(mob.etype, source);
    if !matches!(mob.etype, 31 | 88 | 116 | 119) {
        return true;
    }
    let mut states = lk(live_mob_states());
    let state = states
        .entry(mob.eid)
        .or_insert_with(|| LiveMobState::new(mob.etype));
    match &mut state.behavior {
        Some(MobBehaviorState::Wither(wither)) => !wither.immune_to(hit),
        Some(MobBehaviorState::EnderDragon(dragon)) => {
            !matches!(hit, BossHit::Arrow | BossHit::Trident) || !dragon.is_sitting()
        }
        Some(MobBehaviorState::Shulker(shulker)) => {
            !matches!(hit, BossHit::Arrow) || shulker.peek() != 0
        }
        Some(MobBehaviorState::Warden(warden)) => {
            if source > 0 {
                warden.anger.add_direct_hit(source);
            }
            true
        }
        _ => true,
    }
}

/// A typed Vanilla 1.21.1 damage source. `damage_type` is the bare
/// `minecraft:damage_type` registry id; `cause`/`direct` are entity ids the
/// Damage Event packet reports (`id + 1` on the wire, `0` when absent).
#[derive(Clone, Copy)]
pub(crate) struct DamageSource<'a> {
    pub damage_type: &'a str,
    pub cause: Option<i32>,
    pub direct: Option<i32>,
}

impl<'a> DamageSource<'a> {
    pub(crate) const fn new(damage_type: &'a str) -> Self {
        Self {
            damage_type,
            cause: None,
            direct: None,
        }
    }
    pub(crate) const fn with_cause(mut self, cause: i32) -> Self {
        self.cause = Some(cause);
        self
    }
    pub(crate) const fn with_direct(mut self, direct: i32) -> Self {
        self.direct = Some(direct);
        self
    }
}

fn damage_source_flags(source: &DamageSource<'_>) -> DamageTypeFlags {
    damage_type_flags(source.damage_type)
}

/// `LivingEntity.checkTotemDeathProtection`'s source gate: `out_of_world` is
/// the void arm, `generic_kill` (and the `/kill` pending flag) the kill arm,
/// everything else ordinary.
fn damage_source_totem_kind(c: &Client, source: &DamageSource<'_>) -> TotemDamageKind {
    if source.damage_type == "generic_kill" || c.generic_kill_pending.load(Ordering::SeqCst) {
        return TotemDamageKind::Kill;
    }
    if source.damage_type == "out_of_world" {
        return TotemDamageKind::Void;
    }
    TotemDamageKind::Ordinary
}

/// `LivingEntity.hurt`'s cooldown for sources that do not carry
/// `bypasses_invulnerability`; bypassing sources still report a fresh hurt
/// window so the Damage Event and hurt animation fire.
fn damage_after_invulnerability_typed(
    c: &Client,
    amount: f32,
    bypasses_invulnerability: bool,
) -> Option<(f32, bool)> {
    if bypasses_invulnerability {
        return Some((amount, true));
    }
    damage_after_invulnerability(c, amount)
}

/// `LivingEntity.actuallyHurt` for a typed source: armor absorb is skipped for
/// `bypasses_armor`, the protection-family EPF reduction follows the source's
/// fire/explosion/projectile/fall tags, and `bypasses_invulnerability` /
/// `starve` sources take the raw amount.
fn damage_source_mitigated(c: &Client, raw: f32, source: &DamageSource<'_>) -> f32 {
    let flags = damage_source_flags(source);
    if flags.bypasses_invulnerability || source.damage_type == "starve" {
        return raw;
    }
    let after_armor = if flags.bypasses_armor {
        f64::from(raw)
    } else {
        f64::from(raw) * armor_dmg_factor(c, raw)
    };
    // `LivingEntity#getDamageAfterMagicAbsorb`: Resistance runs after armor
    // and before EPF. 1.21.1's BYPASSES_RESISTANCE tag is the void/kill pair.
    let resistance = if matches!(source.damage_type, "out_of_world" | "generic_kill") {
        1.0
    } else {
        resistance_damage_factor(c)
    };
    let after_resistance = after_armor * resistance;
    (after_resistance * (1.0 - 0.04 * damage_type_epf(c, &flags) as f64)) as f32
}

fn resistance_damage_factor(c: &Client) -> f64 {
    let amp = lk(&c.effs)
        .iter()
        .filter(|e| e.left > 0 && e.id == 10)
        .map(|e| i64::from(e.amp))
        .max();
    amp.map_or(1.0, |amp| {
        ((25.0 - 5.0 * (amp as f64 + 1.0)) / 25.0).max(0.0)
    })
}

fn explosion_knockback_resist_of(c: &Client) -> f64 {
    refresh_equip_attrs(c);
    let attr = attr_of(&format!("p:{}", c.name), "generic.explosion_knockback_resistance");
    let blast = {
        let inv = lk(&c.inv);
        [5i16, 6, 7, 8]
            .iter()
            .filter_map(|slot| inv.get(slot))
            .map(|(_, _, comps)| enchant_level(comps, 3))
            .sum::<i64>()
    };
    (attr + 0.15 * blast as f64).clamp(0.0, 1.0)
}

/// Protocol 767 Damage Event (`0x1a`) body for one victim. The type id comes
/// from the shared `minecraft:damage_type` wire table; absent cause/direct
/// entities encode as `0`, and entity sources carry no source position.
fn damage_event_packet(victim_eid: i32, source: &DamageSource<'_>) -> Option<Vec<u8>> {
    let type_id = damage_type_wire_id(source.damage_type)?;
    let mut packet = Vec::new();
    write_varint(&mut packet, i64::from(victim_eid));
    write_varint(&mut packet, i64::from(type_id));
    write_varint(
        &mut packet,
        source
            .cause
            .filter(|id| *id >= 0)
            .map_or(0, |id| i64::from(id) + 1),
    );
    write_varint(
        &mut packet,
        source
            .direct
            .filter(|id| *id >= 0)
            .map_or(0, |id| i64::from(id) + 1),
    );
    packet.push(0); // No explicit source position for these entity/environment sources.
    Some(packet)
}

/// Send the Damage Event to the victim and every same-dimension client already
/// tracking the victim entity (vanilla's `broadcast` to entity trackers).
fn broadcast_damage_event(victim: &Arc<Client>, source: &DamageSource<'_>) {
    let dim = *lk(&victim.dim);
    let victim_eid = victim.eid;
    for viewer in client_snapshot() {
        if *lk(&viewer.dim) != dim {
            continue;
        }
        if viewer.eid != victim_eid && !lk(&viewer.seen).contains(&victim_eid) {
            continue;
        }
        if let Some(body) = damage_event_packet(victim_eid, source) {
            if let Ok(mut sock) = viewer.sock.lock() {
                let _ = send_packet(&mut sock, 0x1a, body);
            }
        }
    }
}

#[cfg(all(test, feature = "mod-suite"))]
fn live_mob_damage_player(
    player: &Arc<Client>,
    origin: [f64; 3],
    damage: f32,
    _protection_kind: Option<u8>,
    knockback_y: Option<f64>,
) -> bool {
    live_mob_damage_player_typed(
        player,
        origin,
        damage,
        &DamageSource::new("generic"),
        knockback_y,
    )
}

/// Vanilla `SonicBoom` behavior tick: once the 10.0 `sonic_boom` hit lands,
/// the target is pushed along the normalized warden-head → target-eye
/// direction — horizontal components `2.5`, vertical `0.5`, each scaled by
/// `(1 - generic.knockback_resistance)`. `origin` is the warden's feet
/// position; `cause` resolves its eye height for the head anchor.
fn sonic_boom_velocity(
    player: &Arc<Client>,
    origin: [f64; 3],
    cause: Option<i32>,
    kb_res: f64,
) -> Option<Vec<u8>> {
    let head_y = cause
        .and_then(|eid| {
            lk(mobs())
                .iter()
                .find(|mob| mob.eid == eid)
                .map(|mob| f64::from(entity_eye_height_1211(mob.etype)))
        })
        .unwrap_or(0.0);
    let source = [origin[0], origin[1] + head_y, origin[2]];
    let (px, py, pz, ..) = *lk(&player.pos);
    let dim = *lk(&player.dim);
    let target = [px, client_eye_y(player, dim, px, py, pz), pz];
    let dir = [
        target[0] - source[0],
        target[1] - source[1],
        target[2] - source[2],
    ];
    let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
    if len <= 1e-9 {
        return Some(velocity_pkt(player.eid, 0, 0, 0));
    }
    let kr = (1.0 - kb_res).clamp(0.0, 1.0);
    let (dx, dy, dz) = (dir[0] / len, dir[1] / len, dir[2] / len);
    Some(velocity_pkt(
        player.eid,
        (dx * 2.5 * kr * 8000.0) as i16,
        (dy * 0.5 * kr * 8000.0) as i16,
        (dz * 2.5 * kr * 8000.0) as i16,
    ))
}

fn live_mob_damage_player_typed(
    player: &Arc<Client>,
    origin: [f64; 3],
    damage: f32,
    source: &DamageSource<'_>,
    knockback_y: Option<f64>,
) -> bool {
    let flags = damage_source_flags(source);
    if !(flags.bypasses_invulnerability || matches!(*lk(&player.gm), 0 | 2))
        || *lk(&player.hp) <= 0.0
    {
        return false;
    }
    // Mod-suite `EntityDamagePre` (the `Entity.hurt` HEAD equivalent): lifted
    // Mixin bindings observe the hit and verified `onDamage` handlers may
    // rewrite the amount or cancel it outright before the shield gate.
    // `None` = cancelled; zero-mod builds pay one relaxed atomic load.
    let Some(damage) = live_mod_entity_damage_pre(damage) else {
        return false;
    };
    if !flags.bypasses_shield {
        // Vanilla `isDamageSourceBlocked`: raise warmup, 180° attacker arc and
        // the running disable cooldown all gate the block before durability.
        let eval = evaluate_shield_block(&shield_hit_ctx(player, origin, damage));
        if eval.blocked && *lk(&player.blocking) {
            damage_shield_attempts(player, eval.durability_attempts);
            return false;
        }
    }
    if damage <= 0.0 {
        return false;
    }
    let Some((accepted, _)) = damage_after_invulnerability_typed(
        player,
        damage,
        flags.bypasses_invulnerability,
    ) else {
        return false;
    };
    let armor_basis = (!flags.bypasses_armor).then_some(damage);
    player_hurt_apply(player, accepted, source, armor_basis);
    // Vanilla LivingEntity.knockback scales the horizontal push by
    // (1 - knockback_resistance); the vertical component is unaffected.
    // The warden sonic boom is the exception: `SonicBoom` behavior applies
    // its own `push` — horizontal `2.5`, vertical `0.5`, both scaled by
    // (1 - generic.knockback_resistance) along the normalized
    // warden-head → target-eye direction — and no LivingEntity knockback.
    let kb_res = knockback_resist_of(player);
    let velocity = if source.damage_type == "sonic_boom" {
        sonic_boom_velocity(player, origin, source.cause, kb_res)
    } else {
        knockback_y.map(|up| {
            let (x, _, z, ..) = *lk(&player.pos);
            let [dx, _, dz] = mob_behavior_unit([x - origin[0], 0.0, z - origin[2]]);
            velocity_pkt(
                player.eid,
                (dx * 3200.0 * (1.0 - kb_res)) as i16,
                (up * 8000.0) as i16,
                (dz * 3200.0 * (1.0 - kb_res)) as i16,
            )
        })
    };
    if let Some(packet) = &velocity {
        if let Ok(mut sock) = player.sock.lock() {
            let _ = send_packet(&mut sock, 0x5a, packet.clone());
        }
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
        if eval.blocked && *lk(&player.blocking) {
            damage_shield_attempts(player, eval.durability_attempts);
            if eval.disable_ticks.is_some() {
                apply_shield_disable(player);
            }
            return;
        }
    }
    if !live_mob_damage_player_typed(
        player,
        pos,
        diff_mob_dmg(amount),
        &DamageSource::new("mob_attack").with_cause(mob.eid),
        Some(up),
    ) {
        return;
    }
    let thorns = {
        let inv = lk(&player.inv);
        (5i16..=8)
            .filter_map(|slot| inv.get(&slot))
            .map(|stack| enchant_level(&stack.2, 38))
            .sum::<i64>()
    };
    if thorns > 0 && world_rng::next_u64() % 100 < (thorns * 15) as u64 {
        *lk(&mob.hp) -= (1 + world_rng::next_u64() % 4) as f32;
        *lk(&mob.hurt_at) = Instant::now();
        *lk(&mob.last_hurt_tick) = Some(TICK_IDX.load(Ordering::Relaxed));
        broadcast_seen(mob.eid, |_| (0x24, hurt_anim(mob.eid)));
    }
    if *lk(&player.hp) <= 0.0
        && GR_SHOW_DEATH_MESSAGES.load(Ordering::Relaxed) != 0
    {
        // Vanilla `DamageSources.mobAttack`: the attacker's display name is a
        // custom name when one is set, and a custom-named held item picks the
        // `... using <item>` line.
        let name = lk(mob_custom_names())
            .get(&mob.eid)
            .cloned()
            .unwrap_or_else(|| {
                entity_spec_by_id(mob.etype).map_or("mob".to_owned(), |spec| {
                    spec.name
                        .strip_prefix("minecraft:")
                        .unwrap_or(spec.name)
                        .to_owned()
                })
            });
        let weapon = lk(mob_equipment())
            .get(&mob.eid)
            .and_then(|eq| renamed_custom_name(&eq.get(EquipmentSlot::MainHand).2));
        let line = system_chat(&format_vanilla_death_message(
            &player.name,
            "mob_attack",
            Some(&name),
            weapon.as_deref(),
        ));
        broadcast(-1, |_| (0x6c, line.clone()));
    }
}

fn live_mob_explosion(dim: DimId, pos: [f64; 3], power: f32, _players: &[LiveMobPlayer<'_>]) {
    explode_at_dim_griefing(
        dim,
        pos[0],
        pos[1],
        pos[2],
        f64::from(power),
        GR_MOB_GRIEFING.load(Ordering::Relaxed) != 0,
    );
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
    if !matches!(*lk(&client.gm), 0 | 2) || *lk(&client.hp) <= 0.0 {
        return;
    }
    let ticks = server_ticks(u64::from(ticks)) as i32;
    let mut effects = lk(&client.effs);
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
    lk(sitting_cats()).remove(&mob.eid);
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
        "trial_omen" => 33,
        "raid_omen" => 34,
        "wind_charged" => 35,
        "weaving" => 36,
        "oozing" => 37,
        "infested" => 38,
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
    // Fail closed: a client whose registry never yielded a damage-type wire id
    // cannot decode this packet, so it is skipped rather than sent a 0.
    let mut shared = None;
    for client in client_snapshot().iter() {
        let Some(damage_id) = client.player_attack_damage_id else {
            continue;
        };
        if *lk(&client.dim) != dim || !(client.eid == victim || lk(&client.seen).contains(&victim))
        {
            continue;
        }
        let mut body = Vec::new();
        write_varint(&mut body, i64::from(victim));
        write_varint(&mut body, i64::from(damage_id));
        // Minecraft's optional entity IDs encode the ID plus one, reserving zero for None.
        write_varint(&mut body, i64::from(attacker) + 1);
        write_varint(&mut body, i64::from(attacker) + 1);
        body.push(0); // No explicit source position for an entity attack.
        if let Ok(mut w) = client.sock.lock() {
            let _ = send_broadcast_frame(&mut w, &mut shared, 0x1a, &body);
        }
    }
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
    let (last_x, _, last_z) = std::mem::replace(&mut *lk(&c.lastp), (x, y, z));
    let distance = (x - last_x).hypot(z - last_z);
    let movement = if distance >= 10.0 {
        0.0
    } else if *lk(&c.sprinting) {
        distance as f32 * 0.1
    } else if *lk(&c.sneaking) {
        // Vanilla `generic.sneaking_speed` (Swift Sneak, id 37): the sneak
        // crawl rises from 0.3 by 0.15 per level (cap 1.0). Exhaustion tracks
        // the distance covered, normalized on the 0.3 base crawl so an
        // unenchanted sneak accrues exactly what it did before.
        distance as f32 * 0.005 * sneak_movement_speed_multiplier(c) / 0.3
    } else {
        0.0
    };
    // Vanilla per-tick enchant consumers that ride the movement hook:
    // Frost Walker's `onEntityMoved` freeze around grounded feet and the
    // fishing rod's Lure / Luck of the Sea on the owner's live bobber.
    frost_walker_freeze(c);
    apply_fishing_enchants(c);
    let hunger = lk(&c.effs)
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
    let mut sessions = lk(player_mechanism_sessions());
    let Some(session) = sessions.get_mut(&c.eid) else {
        return false;
    };
    let (mut food, mut sat, mut acc) = (
        lk(&c.food),
        lk(&c.sat),
        lk(&c.walk_acc),
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
    let effects = std::mem::take(&mut *lk(&c.effs));
    for effect in effects {
        let packet = remove_effect_pkt(c.eid, effect.id);
        if let Ok(mut sock) = c.sock.lock() {
            let _ = send_packet(&mut sock, 0x43, packet.clone());
        }
        broadcast_seen(c.eid, |_| (0x43, packet.clone()));
    }
    *lk(&c.absorb) = 0.0;
    *lk(&c.air) = server_ticks(AIR_SUPPLY_GT) as i32;
    *lk(&c.invulnerable) = 0;
    *lk(&c.last_hurt) = 0.0;
    c.generic_kill_pending.store(false, Ordering::SeqCst);
    *lk(&c.blocking) = false;
    *lk(&c.sprinting) = false;
    *lk(&c.sneaking) = false;
    *lk(&c.fall_fly) = false;
    let mut sessions = lk(player_mechanism_sessions());
    if let Some(session) = sessions.get_mut(&c.eid) {
        session.food_timer = 0;
    }
    lk(attack_tickers()).remove(&c.eid);
    lk(equip_attrs()).remove(&c.name);
    set_effect_max_health_bonus(&c.name, 0.0);
    let data = FoodData::default();
    *lk(&c.food) = data.food_level;
    *lk(&c.sat) = data.saturation;
    *lk(&c.walk_acc) = 0.0;
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

fn dropped_experience_points(level: i32, _progress: f32) -> i64 {
    // Vanilla `Player#getBaseExperienceReward`: seven points per level, capped
    // at 100. Progress inside the level never contributes.
    (i64::from(level.max(0)) * 7).min(100)
}

/// Reconstruct the level/progress pair from the cumulative XP table for the
/// rare no-session fallback. Fifteen levels already exceed the 100-point death
/// drop cap, so the scan need not follow command-inflated levels further.
fn xp_level_progress_from_total(total: i64) -> (i32, f32) {
    if total >= xp_points_for_level(15) {
        return (15, 0.0);
    }
    let mut level = 0i32;
    while level < 15 && total >= xp_points_for_level(level + 1) {
        level += 1;
    }
    let into_level = total - xp_points_for_level(level);
    let needed = xp_needed_for_next_level(level).max(1);
    (level, (into_level as f32 / needed as f32).clamp(0.0, 1.0))
}

fn take_dropped_experience(c: &Client) -> i64 {
    let mut total = lk(&c.xp);
    let previous_total = std::mem::take(&mut *total);
    let mut sessions = lk(player_mechanism_sessions());
    let Some(session) = sessions.get_mut(&c.eid) else {
        let (level, progress) = xp_level_progress_from_total(previous_total);
        return dropped_experience_points(level, progress);
    };
    let (level, progress) = std::mem::take(&mut session.xp_bar);
    dropped_experience_points(level, progress)
}

fn send_player_xp(
    c: &Client,
    update: impl FnOnce(&mut PlayerXpState) -> Result<(), ExperienceSetError>,
) -> Result<(), ExperienceSetError> {
    // Serialize changes and their packets for this player, including concurrent orb pickups.
    let mut total = lk(&c.xp);
    let mut sessions = lk(player_mechanism_sessions());
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
    health_pkt(hp, *lk(&c.food), *lk(&c.sat))
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
    let mut invulnerable = lk(&c.invulnerable);
    let mut last_hurt = lk(&c.last_hurt);
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
    let mut a = lk(&c.absorb);
    if *a <= 0.0 {
        return dmg;
    }
    let r = (dmg - *a).max(0.0);
    *a = (*a - dmg).max(0.0);
    r
}

/// The shared tail of a player hurt: the typed source's armor/enchantment
/// mitigation, absorption, the hp write, the source-gated totem check, armor
/// wear, the hurt/health packets to the victim's own socket and the protocol
/// Damage Event to the victim and its trackers. `armor_basis` is the
/// `hurtArmor` amount (`None` skips wear). Returns the post-totem hp.
fn player_hurt_apply(
    c: &Arc<Client>,
    accepted: f32,
    source: &DamageSource<'_>,
    armor_basis: Option<f32>,
) -> f32 {
    let taken = damage_source_mitigated(c, accepted, source);
    let flags = damage_source_flags(source);
    let mut h = lk(&c.hp);
    *h = (*h - hurt_absorb(c, taken)).max(0.0);
    if *h <= 0.0 {
        drop(h);
        let kind = damage_source_totem_kind(c, source);
        let _ = try_trigger_totem_of_undying(c, 0.0, kind);
        h = lk(&c.hp);
    }
    let hh = *h;
    drop(h);
    if let Some(raw) = armor_basis {
        if !flags.bypasses_armor {
            damage_armor(c, raw);
        }
    }
    if let Ok(mut w) = c.sock.lock() {
        let _ = send_packet(&mut w, 0x24, hurt_anim(c.eid));
        let _ = send_packet(&mut w, 0x5d, health_pkt_c(hh, c));
    }
    broadcast_damage_event(c, source);
    hh
}

/// Environmental `LivingEntity.baseTick` damage (drowning, lava, standing fire,
/// freezing, void, suffocation and contact hazards): the same typed pipeline
/// as combat — the invulnerability window, armor/enchantment mitigation, the
/// source-gated totem check, hurt + health packets and the Damage Event.
/// Returns the post-totem hp, or `None` when the invulnerability window
/// rejected the hit. Callers must not hold the `hp` lock.
pub(crate) fn live_env_damage(c: &Arc<Client>, raw: f32, damage_type: &str) -> Option<f32> {
    // `Entity.checkBelowWorld` gates the void arm on the falling entity's own
    // dimension: below `min_y - 64` only. The caller's cheap `y < -64` pre-test
    // is the shared lower bound of the three vanilla dimensions, so the exact
    // floor is applied here.
    if damage_type == "out_of_world" {
        let (_, y, ..) = *lk(&c.pos);
        if y >= void_damage_floor(lk(&c.dim)) {
            return None;
        }
    }
    let source = DamageSource::new(damage_type);
    let flags = damage_source_flags(&source);
    let (accepted, _) =
        damage_after_invulnerability_typed(c, raw, flags.bypasses_invulnerability)?;
    Some(player_hurt_apply(c, accepted, &source, Some(raw)))
}

// entity attributes: p:<player-name> persistent | e:<eid> transient
fn attrs() -> &'static Mutex<HashMap<String, HashMap<String, f64>>> {
    static R: OnceLock<Mutex<HashMap<String, HashMap<String, f64>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Attribute registry names (1.21.1), namespaced; the table index is the
/// attribute id `attr_key_id`/`attr_key_name` exchange.
const ATTR_KEY_NAMES: [&str; 31] = [
    "minecraft:generic.armor",
    "minecraft:generic.armor_toughness",
    "minecraft:generic.attack_damage",
    "minecraft:generic.attack_knockback",
    "minecraft:generic.attack_speed",
    "minecraft:player.block_break_speed",
    "minecraft:player.block_interaction_range",
    "minecraft:generic.burning_time",
    "minecraft:generic.explosion_knockback_resistance",
    "minecraft:player.entity_interaction_range",
    "minecraft:generic.fall_damage_multiplier",
    "minecraft:generic.flying_speed",
    "minecraft:generic.follow_range",
    "minecraft:generic.gravity",
    "minecraft:generic.jump_strength",
    "minecraft:generic.knockback_resistance",
    "minecraft:generic.luck",
    "minecraft:generic.max_absorption",
    "minecraft:generic.max_health",
    "minecraft:player.mining_efficiency",
    "minecraft:generic.movement_efficiency",
    "minecraft:generic.movement_speed",
    "minecraft:generic.oxygen_bonus",
    "minecraft:generic.safe_fall_distance",
    "minecraft:generic.scale",
    "minecraft:player.sneaking_speed",
    "minecraft:zombie.spawn_reinforcements",
    "minecraft:generic.step_height",
    "minecraft:player.submerged_mining_speed",
    "minecraft:player.sweeping_damage_ratio",
    "minecraft:generic.water_movement_efficiency",
];
fn attr_key_id(name: &str) -> Option<i64> {
    let n = name.trim_start_matches("minecraft:");
    ATTR_KEY_NAMES
        .iter()
        .position(|k| k.strip_prefix("minecraft:") == Some(n))
        .map(|i| i as i64)
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
    usize::try_from(id)
        .ok()
        .and_then(|i| ATTR_KEY_NAMES.get(i))
        .copied()
}
fn attr_of(key: &str, name: &str) -> f64 {
    let base = lk(attrs())
        .get(key)
        .and_then(|m| m.get(name))
        .copied()
        .unwrap_or_else(|| attr_default(name));
    // Vanilla attribute instances resolve equipment modifiers on top of the
    // base. The equipment side is cached (computed without taking locks here:
    // some callers hold the health lock, which orders after inventory).
    if let (Some(id), Some(pname)) = (attr_key_id(name), key.strip_prefix("p:")) {
        if let Some((add, mb, mt)) = lk(equip_attrs())
            .get(pname)
            .and_then(|m| m.get(&(id as i32)))
            .copied()
        {
            return (base + add) * (1.0 + mb) * mt;
        }
    }
    base
}
/// The live `generic.max_health` bonus `MobEffects.HEALTH_BOOST` contributes
/// (`+4.0` per effect level, `ADD_VALUE`), republished every effect tick by
/// `effects::tick_effects` so `max_hp_of` sees it everywhere attributes,
/// food healing and respawn reads do.
fn effect_max_health_bonuses() -> &'static Mutex<HashMap<String, f32>> {
    static BONUSES: OnceLock<Mutex<HashMap<String, f32>>> = OnceLock::new();
    BONUSES.get_or_init(Mutex::default)
}

/// Stores the live Health Boost bonus and returns the previous total, so the
/// effect tick only pays the health-lock clamp pass when the bonus shrank.
pub(crate) fn set_effect_max_health_bonus(pname: &str, bonus: f32) -> f32 {
    let mut bonuses = effect_max_health_bonuses()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = bonuses.get(pname).copied().unwrap_or(0.0);
    if bonus > 0.0 {
        bonuses.insert(pname.to_owned(), bonus);
    } else {
        bonuses.remove(pname);
    }
    previous
}

fn max_hp_of(pname: &str) -> f32 {
    attr_of(&format!("p:{}", pname), "generic.max_health") as f32
        + effect_max_health_bonuses()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(pname)
            .copied()
            .unwrap_or(0.0)
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
    let m = lk(attrs())
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
mod xp_death_drop_tests {
    use super::*;
    use crate::effect_tests::client_pair;

    #[test]
    fn p1_combat_firework_damage_matches_vanilla_star_scaling() {
        assert_eq!(firework_damage_base(1), 7.0);
        assert_eq!(firework_damage_base(3), 11.0);
        assert_eq!(firework_damage_base(7), 19.0);
    }

    #[test]
    fn combat_death_xp_formula_matches_vanilla() {
        let _guard = crate::world_guard();
        let (mut client, _receiver) = client_pair();
        client.eid = 84_242;
        let eid = client.eid;
        struct SessionCleanup(i32);
        impl std::ops::Drop for SessionCleanup {
            fn drop(&mut self) {
                lk(player_mechanism_sessions())
                    .remove(&self.0);
            }
        }
        let _cleanup = SessionCleanup(eid);
        lk(player_mechanism_sessions())
            .insert(
                eid,
                PlayerMechanismSession {
                    xp_bar: (0, 0.0),
                    food_timer: 0,
                    menus: Arc::new(LiveMenuConnection {
                        active: Mutex::new(None),
                        next_id: std::sync::atomic::AtomicU8::new(4),
                        enchantment_seed: Mutex::new(0),
                    }),
                },
            );

        for (level, progress) in [
            (0, 0.5_f32),
            (1, 0.5),
            (6, 0.25),
            (7, 0.25),
            (8, 0.75),
            (30, 0.75),
        ] {
            {
                let mut sessions = lk(player_mechanism_sessions());
                sessions.get_mut(&eid).unwrap().xp_bar = (level, progress);
            }
            *lk(&client.xp) = 500;
            let actual = take_dropped_experience(&client);
            let expected = (i64::from(level) * 7).min(100);
            assert_eq!(actual, expected, "level {level} progress {progress}");
            assert_eq!(dropped_experience_points(level, progress), expected);
            assert_eq!(*lk(&client.xp), 0);
            assert_eq!(
                lk(player_mechanism_sessions())
                    .get(&eid)
                    .unwrap()
                    .xp_bar,
                (0, 0.0)
            );
        }
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
            lk(attack_tickers()).remove(&4242);
            lk(equip_attrs()).remove("totem-test");
            lk(mobs()).retain(|m| m.eid != 4242);
            lk(mace_smashes()).remove(&4242);
            lk(clients())
                .retain(|c| c.eid != 4242);
        }
    }

    #[test]
    fn punch_arrow_sends_teleport_to_a_tracking_client() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        let _guard = crate::world_guard();
        let _cleanup = CombatGuard::new();
        let (client, mut receiver) = client_pair();
        let mob = test_mob(4243);
        *lk(&mob.pos) = (9.0, 65.0, 8.5, 90.0, 15.0);
        lk(&client.seen).insert(mob.eid);
        lk(clients()).push(Arc::new(client));

        apply_arrow_punch(&mob, 2.0, 0.0, 2);

        assert_eq!(
            read_packet(&mut receiver, Instant::now() + Duration::from_secs(1)),
            Some((0x70, entity_teleport(mob.eid, 9.6, 65.0, 8.5, 90.0, 15.0, true)))
        );
    }

    fn enchanted_stack(item: &str, ench: &[(i64, i64)]) -> Stack {
        let mut comps = Vec::new();
        comp_set(&mut comps, COMP_ENCHANTMENTS, enchantments_data(ench, true));
        (name_to_item(item).unwrap(), 1, comps)
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
            modifier_id: "megu:test".to_string(),
            amount,
            operation: op,
            slot,
        }
    }

    fn test_mob(eid: i32) -> Arc<Mob> {
        test_mob_etype(eid, 124)
    }

    fn test_mob_etype(eid: i32, etype: i64) -> Arc<Mob> {
        Arc::new(Mob {
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
        })
    }

    #[test]
    fn p1_combat_sharpness_levels_and_crit_enchant_order() {
        assert_eq!(vanilla_sharpness_damage(0), 0.0);
        assert_eq!(vanilla_sharpness_damage(1), 1.0);
        assert_eq!(vanilla_sharpness_damage(5), 3.0);
        // A full-charge diamond-sword crit with Sharpness I is
        // 7*1.5 + 1.0 = 11.5, never (7+1)*1.5 = 12.0.
        assert_eq!(melee_hit_damage(7.0, 1.0, 1.0, true), 11.5);
        // At half charge a 4-point mace smash is added after the charge
        // scale but still receives the crit factor: 7*.4=2.8, +4, *1.5, +0.5.
        assert!((melee_hit_damage_with_bonus(7.0, 1.0, 0.5, true, 4.0) - 10.7).abs() < 1e-5);
        assert!(!is_vanilla_critical_hit(&CritContext {
            attack_charge: 0.5,
            fall_distance: 1.0,
            target_is_living: true,
            ..Default::default()
        }));
    }

    #[test]
    fn p1_combat_resistance_levels_scale_damage_by_twenty_percent() {
        let _guard = crate::world_guard();
        let (client, _rx) = client_pair();
        let source = DamageSource::new("mob_attack");
        for (amp, want) in [
            (0, 16.0),
            (1, 12.0),
            (2, 8.0),
            (3, 4.0),
            (4, 0.0),
        ] {
            *lk(&client.effs) = vec![Eff {
                id: 10,
                amp,
                left: 20,
            }];
            let got = damage_source_mitigated(&client, 20.0, &source);
            assert!((got - want).abs() < 1e-5, "amp {amp}: {got} vs {want}");
        }
        lk(&client.effs).clear();
    }

    #[test]
    fn p1_combat_explosion_knockback_resistance_reads_blast_protection() {
        let _guard = crate::world_guard();
        let (client, _rx) = client_pair();
        assert_eq!(
            attr_key_id("minecraft:generic.explosion_knockback_resistance"),
            Some(8)
        );
        assert_eq!(explosion_knockback_resist_of(&client), 0.0);
        lk(&client.inv).insert(5, enchanted_stack("diamond_helmet", &[(3, 4)]));
        assert!((explosion_knockback_resist_of(&client) - 0.6).abs() < 1e-9);
        lk(&client.inv).insert(6, enchanted_stack("diamond_chestplate", &[(3, 4)]));
        assert_eq!(explosion_knockback_resist_of(&client), 1.0);
    }

    #[test]
    fn p1_combat_pvp_shield_helper_requires_frontal_arc_and_warmup() {
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let (mut attacker, _rxa) = client_pair();
        attacker.eid = 4241;
        let (mut victim, _rxv) = client_pair();
        victim.eid = 4242;
        *lk(&victim.pos) = (0.0, 64.0, 0.0, 0.0, 0.0, true);
        *lk(&victim.blocking) = true;
        let victim = Arc::new(victim);
        let held = enchanted_stack("diamond_axe", &[]);
        let calc = MeleeAttackCalc {
            kind: 3,
            charge: 1.0,
            critical: false,
            base: 9.0,
            enchant_bonus: 0.0,
            mace_fall: 0.0,
            smash_bonus: 0.0,
        };
        TICK_IDX.store(100, Ordering::Relaxed);
        lk(shield_raised()).insert(victim.eid, 90);
        // Yaw 0 faces +Z. A +Z attacker is inside the open frontal hemisphere;
        // the same hit from -Z is not a block at all.
        *lk(&attacker.pos) = (0.0, 64.0, 1.0, 0.0, 0.0, true);
        assert!(player_melee_shield_block(&attacker, &victim, &calc, &held, 9.0).blocked);
        *lk(&attacker.pos) = (0.0, 64.0, -1.0, 0.0, 0.0, true);
        assert!(!player_melee_shield_block(&attacker, &victim, &calc, &held, 9.0).blocked);
        // Inside the five-tick use warmup even a frontal hit is not blocked.
        lk(shield_raised()).insert(victim.eid, 99);
        *lk(&attacker.pos) = (0.0, 64.0, 1.0, 0.0, 0.0, true);
        assert!(!player_melee_shield_block(&attacker, &victim, &calc, &held, 9.0).blocked);
        lk(shield_raised()).remove(&victim.eid);
    }

    #[test]
    fn p1_combat_sweep_damage_uses_post_charge_base_and_enchants() {
        let calc = MeleeAttackCalc {
            kind: 0,
            charge: 0.95,
            critical: false,
            base: 7.0,
            enchant_bonus: 1.0,
            mace_fall: 0.0,
            smash_bonus: 0.0,
        };
        let held = enchanted_stack("diamond_sword", &[(32, 1), (36, 3)]);
        let target = player_combat_box(0.0, 64.0, 0.0);
        let candidate = player_combat_box(0.8, 64.0, 0.0);
        let env = MeleeSweepEnv {
            calc: &calc,
            held: &held,
            attacker_pos: [0.0, 64.0, 0.0],
            on_ground: true,
            sprinting: false,
            target_box: target,
        };
        let got = melee_sweep_damage(&env, candidate, None).unwrap();
        let scaled_base = 7.0 * (0.2 + 0.8 * 0.95 * 0.95);
        let want = (1.0 + 0.75 * scaled_base + 1.0) * 0.95;
        assert!((got - want as f32).abs() < 1e-4, "{got} vs {want}");
    }

    #[test]
    fn p1_combat_attack_speed_table_matches_vanilla_defaults() {
        let close = |name: &str, want: f64| {
            let item = name_to_item(name).unwrap();
            let got = attack_cooldown(item);
            assert!((got - want).abs() < 1e-9, "{name}: {got} vs {want}");
        };
        close("wooden_axe", 1.25);
        close("stone_axe", 1.25);
        close("golden_axe", 1.0);
        close("iron_axe", 1.0 / 0.9);
        close("diamond_axe", 1.0);
        close("netherite_axe", 1.0);
        close("wooden_hoe", 1.0);
        close("golden_hoe", 1.0);
        close("stone_hoe", 0.5);
        close("iron_hoe", 1.0 / 3.0);
        close("diamond_hoe", 0.25);
        close("netherite_hoe", 0.25);
        close("diamond_pickaxe", 1.0 / 1.2);
        close("diamond_shovel", 1.0);
        close("trident", 1.0 / 1.1);
        close("mace", 1.0 / 0.6);
    }

    #[test]
    fn p1_combat_attack_cooldown_counts_game_ticks() {
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        let sword = name_to_item("diamond_sword").unwrap();
        lk(&client.inv).insert(36, (sword, 1, Vec::new()));
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
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        let sword = name_to_item("diamond_sword").unwrap();
        lk(&client.inv).insert(36, (sword, 1, Vec::new()));
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
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        let sword = name_to_item("diamond_sword").unwrap();
        lk(&client.inv).insert(36, (sword, 1, Vec::new()));
        lk(&client.inv).insert(37, (sword, 1, Vec::new()));
        TICK_IDX.store(100, Ordering::Relaxed);
        assert_eq!(melee_charge(&client, sword), 1.0);
        TICK_IDX.store(112, Ordering::Relaxed);
        // 12 ticks of charge (0.76) on the old item, then vanilla
        // resetAttackStrengthTicker on the switch: the new item starts at 0.
        *lk(&client.held) = 1;
        let c = f64::from(melee_charge(&client, sword));
        assert!((c - 0.5 / 12.5).abs() < 1e-6, "{c}");
    }

    #[test]
    fn p1_combat_mob_hurt_window_runs_on_ticks() {
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let mob = test_mob(4242);
        lk(mobs()).push(mob.clone());
        TICK_IDX.store(100, Ordering::Relaxed);
        // The melee gate writes only the wall stamp; the tick pass adopts it.
        *lk(&mob.hurt_at) = Instant::now();
        mob_hurt_window_tick();
        assert_eq!(*lk(&mob.last_hurt_tick), Some(100));
        assert!(
            lk(&mob.hurt_at).elapsed() < Duration::from_millis(500),
            "window must be shut ten ticks"
        );
        // Frozen ticks: even after the wall window would have lapsed, the gate
        // stays shut.
        std::thread::sleep(Duration::from_millis(600));
        assert!(lk(&mob.hurt_at).elapsed() < Duration::from_millis(500));
        // Ten game ticks later the window ends and the wall stamp is backed out
        // to the hurt instant (elapsed >= the 500ms gate).
        TICK_IDX.store(110, Ordering::Relaxed);
        mob_hurt_window_tick();
        assert!(lk(&mob.hurt_at).elapsed() >= Duration::from_millis(500));
    }

    #[test]
    fn p1_combat_mob_hurt_window_uses_vanilla_damage_differential() {
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let mob = test_mob(4242);
        lk(mobs()).push(mob.clone());
        TICK_IDX.store(100, Ordering::Relaxed);
        assert_eq!(mob_melee_hurt_gate(&mob, 7.0), Some(7.0));
        assert_eq!(mob_melee_hurt_gate(&mob, 5.0), None);
        assert_eq!(mob_melee_hurt_gate(&mob, 8.0), Some(1.0));
        TICK_IDX.store(110, Ordering::Relaxed);
        assert_eq!(mob_melee_hurt_gate(&mob, 4.0), Some(4.0));
    }

    #[test]
    fn p1_combat_mob_armor_epf_breach_and_resistance_pipeline() {
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let mob = test_mob(4242);
        lk(mobs()).push(mob.clone());
        let mut equipment = EntityEquipment::default();
        equipment.set(
            EquipmentSlot::Chest,
            enchanted_stack("diamond_chestplate", &[(27, 4)]),
        );
        lk(mob_equipment()).insert(mob.eid, equipment);
        lk(mob_effects()).insert(
            mob.eid,
            vec![Eff {
                id: 10,
                amp: 0,
                left: 20,
            }],
        );
        // At damage 20 the diamond chestplate's vanilla armor formula bottoms
        // out at armor/5 = 1.6 points (a 6.4% reduction); Resistance I leaves
        // 80%, and Protection IV's EPF 4 leaves 84% of that post-armor damage.
        let got = mob_melee_mitigate(&mob, 20.0, 0);
        let want = 20.0 * (1.0 - 1.6 / 25.0) * 0.8 * 0.84;
        assert!((got - want as f32).abs() < 1e-4, "{got} vs {want}");
        let breached = mob_melee_mitigate(&mob, 20.0, 4);
        let breach_want = 20.0 * (1.0 - (1.6 / 25.0) * 0.4) * 0.8 * 0.84;
        assert!((breached - breach_want as f32).abs() < 1e-4);
        lk(mob_equipment()).remove(&mob.eid);
        lk(mob_effects()).remove(&mob.eid);
    }

    #[test]
    fn p1_combat_mob_hurt_window_stamped_paths_hold() {
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let mob = test_mob(4242);
        lk(mobs()).push(mob.clone());
        TICK_IDX.store(100, Ordering::Relaxed);
        *lk(&mob.last_hurt_tick) = Some(100);
        *lk(&mob.hurt_at) = Instant::now();
        mob_hurt_window_tick();
        TICK_IDX.store(109, Ordering::Relaxed);
        mob_hurt_window_tick();
        assert!(
            lk(&mob.hurt_at).elapsed() < Duration::from_millis(500),
            "still inside the 10-tick window at tick 109"
        );
        TICK_IDX.store(110, Ordering::Relaxed);
        mob_hurt_window_tick();
        assert!(lk(&mob.hurt_at).elapsed() >= Duration::from_millis(500));
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
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        let sword = name_to_item("diamond_sword").unwrap();
        // +3 attack damage and a slower attack speed (1.2 < 1.6 table speed).
        lk(&client.inv).insert(
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
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        assert_eq!(knockback_resist_of(&client), 0.0);
        lk(&client.inv).insert(
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
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let client = Arc::new(client);
        assert_eq!(max_hp_of(&client.name), 20.0);
        lk(&client.inv).insert(
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

    #[test]
    fn p1_combat_density_is_hit_time_damage() {
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        let mace = enchanted_stack("mace", &[(6, 2)]);
        *lk(&client.pos) =
            (0.0, 63.5, 0.0, 0.0, 0.0, false);
        *lk(&client.fall_from) = 70.0;
        lk(&client.inv).insert(36, mace.clone());
        refresh_equip_attrs(&client);
        assert_eq!(attr_of(&format!("p:{}", client.name), "generic.attack_damage"), 1.0);
        let bonus = |item| mace_smash_damage_bonus(
            mace_attack_fall(&client, item), enchant_level(&mace.2, 6) as i32,
        );
        assert_eq!(bonus(mace.0), 25.5);
        assert_eq!(bonus(name_to_item("diamond_sword").unwrap()), 0.0);
        lk(&client.pos).5 = true;
        assert_eq!(bonus(mace.0), 0.0);
        *lk(&client.pos) =
            (0.0, 69.0, 0.0, 0.0, 0.0, false);
        assert_eq!(bonus(mace.0), 0.0);
    }

    #[test]
    fn p1_combat_wind_burst_smash_bounces_resets_fall_and_pulses() {
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let (client, mut rx) = client_pair();
        // Airborne 4 blocks into a fall, Wind Burst I mace in hand.
        {
            let mut p = lk(&client.pos);
            *p = (0.0, 66.0, 0.0, 0.0, 0.0, false);
        }
        *lk(&client.fall_from) = 70.0;
        lk(&client.inv)
            .insert(36, enchanted_stack("mace", &[(41, 1)]));
        let client = Arc::new(client);
        lk(clients())
            .push(client.clone());
        let mob = test_mob(4243);
        *lk(&mob.pos) = (0.4, 66.4, 0.0, 0.0, 0.0);
        lk(mobs()).push(mob.clone());

        let mace = name_to_item("mace").unwrap();
        melee_charge(&client, mace);
        assert!(lk(mace_smashes()).is_empty());
        assert_eq!(*lk(&client.fall_from), 70.0);
        let held = enchanted_stack("mace", &[(41, 1)]);
        let fall = mace_attack_fall(&client, mace);
        queue_mace_smash(&client, &held, fall, 100, [0.0, 66.0, 0.0]);
        assert_eq!(mace_attack_fall(&client, mace), 0.0);
        assert!(
            lk(mace_smashes())
                .contains_key(&4242),
            "a qualifying mace smash queues the wind burst launch"
        );
        combat2_state_tick();
        assert!(
            lk(mace_smashes()).is_empty(),
            "the combat tick drains the queued smash"
        );
        // Fall distance resets to the burst Y so the combo only counts the
        // drop below it.
        assert_eq!(
            *lk(&client.fall_from),
            66.0
        );
        // The wielder gets the level I upward launch (1.2 blocks → vy 9600).
        let packet = read_packet(&mut rx, Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(packet.0, 0x28);
        let packet = read_packet(&mut rx, Instant::now() + Duration::from_secs(1)).unwrap();
        assert_eq!(packet, (0x5a, velocity_pkt(client.eid, 0, (1.2 * 8000.0) as i16, 0)));
        // The radial pulse knocked the nearby mob away from the burst.
        let mp = *lk(&mob.pos);
        assert!(mp.0 > 0.4, "mace smash pulse pushes mobs away, got {}", mp.0);
        lk(mobs()).retain(|m| m.eid != 4243);
    }

    #[test]
    fn p1_combat_wind_burst_requires_airborne_smash_past_trigger() {
        let _guard = crate::world_guard();
        let (client, _rx) = client_pair();
        let mace = name_to_item("mace").unwrap();
        assert_eq!(mace_attack_fall(&client, mace), 0.0);
        lk(&client.pos).5 = false;
        *lk(&client.fall_fly) = true;
        *lk(&client.fall_from) = 80.0;
        assert_eq!(mace_attack_fall(&client, mace), 0.0);
        *lk(&client.fall_fly) = false;
        lk(&client.pos).1 = 79.0;
        assert_eq!(mace_smash_damage_bonus(mace_attack_fall(&client, mace), 3), 0.0);
        // Levels I–III bounce 1.2 / 2.1 / 3.0 blocks.
        assert!((wind_burst_bounce_velocity(1) - 1.2).abs() < 1e-9);
        assert!((wind_burst_bounce_velocity(2) - 1.75).abs() < 1e-9);
        assert!((wind_burst_bounce_velocity(3) - 2.2).abs() < 1e-9);
        assert_eq!(wind_burst_bounce_velocity(0), 0.0);
    }

    #[test]
    fn p1_combat_mace_v1211_tiered_smash_density_breach_and_fall_reset() {
        assert_eq!(mace_smash_bonus_damage(1.4), 5.6);
        assert_eq!(mace_smash_bonus_damage(3.0), 12.0);
        assert_eq!(mace_smash_bonus_damage(8.0), 22.0);
        assert_eq!(mace_smash_bonus_damage(10.0), 24.0);
        assert_eq!(mace_smash_damage_bonus(1.5, 5), 0.0);
        assert!((mace_smash_damage_bonus(6.5, 2) - 25.5).abs() < 1e-6);
        assert!((mace_smash_damage_bonus(6.5, 0) - 19.0).abs() < 1e-6);

        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let (client, _rx) = client_pair();
        lk(&client.inv)
            .insert(5, (name_to_item("diamond_chestplate").unwrap(), 1, Vec::new()));
        *lk(&client.armor_pts) = 20;
        let normal = dmg_after(&client, 20.0, 0);
        let breach = dmg_after_breach(&client, 20.0, 0, 4);
        assert!((breach - 16.16).abs() < 1e-5, "breach IV got {breach}");
        assert!((dmg_after_breach(&client, 20.0, 0, 1) - 11.84).abs() < 1e-5);
        assert_eq!(dmg_after_breach(&client, 20.0, 0, 0), normal);

        // A successful level-0 smash still drains on the combat tick and resets
        // fall distance; only the Wind Burst bounce is absent.
        let (plain, _plain_rx) = client_pair();
        let plain = Arc::new(plain);
        lk(clients())
            .push(plain.clone());
        {
            let mut pos = lk(&plain.pos);
            *pos = (0.0, 60.0, 0.0, 0.0, 0.0, false);
        }
        *lk(&plain.fall_from) = 70.0;
        let held = enchanted_stack("mace", &[]);
        queue_mace_smash(&plain, &held, mace_attack_fall(&plain, held.0), 100, [1.0, 60.0, 0.0]);
        assert_eq!(*lk(&plain.fall_from), 60.0);
        combat2_state_tick();
        assert_eq!(*lk(&plain.fall_from), 60.0);
        assert!(lk(mace_smashes()).is_empty());
    }

    #[test]
    fn p1_combat_warden_direct_hit_adds_twenty_anger() {
        let _guard = crate::world_guard();
        let _g = CombatGuard::new();
        let mob = test_mob_etype(9200, 116);
        lk(mobs()).push(mob.clone());
        let mut states = lk(live_mob_states());
        states.insert(9200, LiveMobState::new(116));
        drop(states);
        assert!(live_mob_hit_allowed(&mob, 33, BossHit::Other));
        let states = lk(live_mob_states());
        let Some(LiveMobState {
            behavior: Some(MobBehaviorState::Warden(warden)),
            ..
        }) = states.get(&9200)
        else {
            panic!("warden behavior missing");
        };
        assert_eq!(warden.anger.entities.get(&33).copied(), Some(20));
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
            lk(mob_fire_ticks()).clear();
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
        let _guard = crate::world_guard();
        let _g = FireGuard::new();
        let creeper = mob(4243, name_to_etype("creeper").unwrap());
        *lk(&creeper.fuse) = 7;
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
        assert_eq!(*lk(&creeper.fuse), 7, "fuse untouched");
        assert_eq!(*lk(&creeper.fire_ticks), 0, "sunlight field untouched");
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
        let _guard = crate::world_guard();
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
        assert!(lk(mob_fire_ticks()).get(&eid).is_none());
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
        let mut smite_v = Vec::new();
        comp_set(
            &mut smite_v,
            COMP_ENCHANTMENTS,
            enchantments_data(&[(ENCHANT_SMITE, 5)], true),
        );
        for name in [
            "zombie", "zombie_villager", "husk", "drowned", "zombified_piglin",
            "zoglin", "skeleton", "stray", "bogged", "wither_skeleton",
            "skeleton_horse", "zombie_horse", "phantom", "wither",
        ] {
            let etype = name_to_etype(name).unwrap();
            assert!(enchantment_targets_undead(etype), "{name}");
            assert_eq!(typed_damage_bonus(&smite_v, etype), 12.5, "{name} smite V");
        }
        for name in ["spider", "creeper", "zoglin_piglin_unknown", "cow", "piglin"] {
            let Some(etype) = name_to_etype(name) else { continue };
            assert!(!enchantment_targets_undead(etype), "{name}");
            assert_eq!(typed_damage_bonus(&smite_v, etype), 0.0, "{name}");
        }
        // The live path's own enchant_level id for smite is 34.
        assert_eq!(enchant_id("minecraft:smite"), Some(34));
    }

    // #81 — Bane's arthropod family plus the Slowness IV rider.
    #[test]
    fn p5a_parity_bane_covers_arthropods_and_slowness_iv() {
        let mut bane_iii = Vec::new();
        comp_set(
            &mut bane_iii,
            COMP_ENCHANTMENTS,
            enchantments_data(&[(ENCHANT_BANE_OF_ARTHROPODS, 3)], true),
        );
        for name in ["spider", "cave_spider", "silverfish", "endermite", "bee"] {
            let etype = name_to_etype(name).unwrap();
            assert!(enchantment_targets_arthropod(etype), "{name}");
            assert_eq!(typed_damage_bonus(&bane_iii, etype), 7.5, "{name} bane III");
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
        let mut impaling_ii = Vec::new();
        comp_set(
            &mut impaling_ii,
            COMP_ENCHANTMENTS,
            enchantments_data(&[(ENCHANT_IMPALING, 2)], true),
        );
        for name in [
            "cod", "salmon", "pufferfish", "tropical_fish", "squid", "glow_squid",
            "dolphin", "turtle", "guardian", "elder_guardian", "axolotl", "tadpole",
        ] {
            let etype = name_to_etype(name).unwrap();
            assert!(enchantment_targets_aquatic(etype), "{name}");
            assert_eq!(typed_damage_bonus(&impaling_ii, etype), 5.0, "{name} impaling II");
        }
        for name in ["drowned", "frog", "guardian_z"] {
            let Some(etype) = name_to_etype(name) else { continue };
            assert!(!enchantment_targets_aquatic(etype), "{name}");
        }
        // The live formula is type-gated: an Impaling II trident pays nothing
        // against a target outside `#minecraft:sensitive_to_impaling`.
        let cow = name_to_etype("cow").unwrap();
        assert_eq!(typed_damage_bonus(&impaling_ii, cow), 0.0, "impaling is aquatic-only");
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
        let _guard = crate::world_guard();
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

#[cfg(all(test, megu_vanilla_data))]
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
            lk(clients())
                .retain(|c| c.eid != 4242);
            lk(arrows())
                .retain(|a| a.eid != 99901);
            lk(drops())
                .retain(|d| d.item != 9999);
            for m in [
                shield_raised(),
                riptide_spin(),
            ] {
                lk(m).remove(&4242);
            }
            lk(trident_returning())
                .remove(&99901);
            lk(trident_seen())
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
        assert_eq!(b.knockback_multiplier, 1.0);
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
        // Breeze: same geometry, reach 6.0, mult 1.0.
        let dv = evaluate_wind_charge_burst(
            WindChargeKind::Breeze,
            [0.0, 0.0, 0.0],
            [0.0, 0.0, 0.0],
            1.0,
            1.0,
            0.0,
        )
        .unwrap();
        let want = 1.0 - 1.0 / 6.0;
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
        let _guard = crate::world_guard();
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        *lk(&c.fall_from) = 120.0;
        wind_charge_reset_fall(&c, 64.5);
        assert_eq!(*lk(&c.fall_from), 64.5);
        // Non-finite burst heights never corrupt the fall baseline.
        wind_charge_reset_fall(&c, f64::NAN);
        assert_eq!(*lk(&c.fall_from), 64.5);
    }

    #[test]
    fn vanilla_combat2_wind_charge_redstone_triggers() {
        let _guard = crate::world_guard();
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
        let _guard = crate::world_guard();
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
        let _guard = crate::world_guard();
        let _g = LaneGuard::new();
        let (c, mut r) = client_pair();
        *lk(&c.fall_from) = 200.0;
        {
            let mut p = lk(&c.pos);
            p.0 = 0.0;
            p.1 = 70.0;
            p.2 = 1.5; // 1.5 blocks north of the burst
        }
        let c = Arc::new(c);
        lk(clients())
            .push(c.clone());
        wind_charge_burst(DimId::Overworld, WindChargeKind::Player, [0.0, 70.0, 0.0], 4242);
        // Thrower fall baseline snapped to the burst Y.
        assert_eq!(
            *lk(&c.fall_from),
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
        let _guard = crate::world_guard();
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
        lk(&c.inv)
            .insert(45, (sh, 1, comps));
        damage_shield_attempts(&c, 5);
        let dmg = comp_varint(
            &lk(&c.inv).get(&45).unwrap().2,
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
        lk(&c.inv)
            .insert(45, (sh, 1, comps));
        damage_shield_attempts(&c, 10);
        assert_eq!(
            comp_varint(
                &lk(&c.inv).get(&45).unwrap().2,
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
        lk(&c.inv)
            .insert(45, (sh, 1, comps));
        damage_shield_attempts(&c, 400);
        let dmg = comp_varint(
            &lk(&c.inv).get(&45).unwrap().2,
            COMP_DAMAGE,
        )
        .unwrap();
        assert!(dmg < 400, "unbreaking must skip attempts, dmg={dmg}");
        // Unbreakable shields take no durability at all.
        let mut comps = Vec::new();
        comp_set_varint(&mut comps, COMP_MAX_DAMAGE, 337);
        comp_set(&mut comps, COMP_UNBREAKABLE, vec![]);
        lk(&c.inv)
            .insert(45, (sh, 1, comps));
        damage_shield_attempts(&c, 20);
        assert_eq!(
            comp_varint(
                &lk(&c.inv).get(&45).unwrap().2,
                COMP_DAMAGE
            ),
            None
        );
    }

    #[test]
    fn vanilla_combat2_shield_state_tick_and_block_gate() {
        let _guard = crate::world_guard();
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        let c = Arc::new(c);
        lk(clients())
            .push(c.clone());
        TICK_IDX.store(100, Ordering::Relaxed);
        combat2_state_tick();
        // Shield down: raised clock stays empty, ctx reports 0 -> below warmup.
        assert_eq!(shield_raised_ticks(&c), 0);
        *lk(&c.blocking) = true;
        combat2_state_tick();
        assert_eq!(shield_raised_ticks(&c), 0); // just raised
        TICK_IDX.store(105, Ordering::Relaxed);
        combat2_state_tick();
        assert_eq!(shield_raised_ticks(&c), 5);
        // apply_shield_disable drops the shield and arms the wall-clock cd.
        apply_shield_disable(&c);
        assert!(!*lk(&c.blocking));
        let ctx = shield_hit_ctx(&c, [5.0, 0.0, 0.0], 5.0);
        assert!(ctx.cooldown_active);
        assert!(!evaluate_shield_block(&ctx).blocked);
    }

    // ---------- totem of undying ----------

    #[test]
    fn vanilla_combat2_totem_offhand_pop_effects_status35() {
        let _guard = crate::world_guard();
        let _g = LaneGuard::new();
        let (c, mut r) = client_pair();
        let totem = name_to_item("totem_of_undying").unwrap();
        lk(&c.inv)
            .insert(45, (totem, 2, Vec::new()));
        lk(&c.effs).push(Eff {
            id: name_to_effect("poison").unwrap_or(19),
            amp: 2,
            left: 80,
        });
        *lk(&c.hp) = 0.0;
        assert!(try_trigger_totem_of_undying(&c, 0.0, TotemDamageKind::Ordinary));
        // Totem consumed (one of two), hp = 1.
        assert_eq!(
            lk(&c.inv).get(&45),
            Some(&(totem, 1, Vec::new()))
        );
        assert_eq!(*lk(&c.hp), 1.0);
        // Negative effect cleared; exact vanilla grant.
        let effs: Vec<(i32, i32, i32)> = lk(&c.effs)
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
        assert_eq!(*lk(&c.absorb), 8.0);
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
        let _guard = crate::world_guard();
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        let totem = name_to_item("totem_of_undying").unwrap();
        *lk(&c.held) = 3;
        lk(&c.inv)
            .insert(39, (totem, 1, Vec::new())); // main hand = 36 + held
        *lk(&c.hp) = -4.0;
        assert!(try_trigger_totem_of_undying(
            &c,
            -4.0,
            TotemDamageKind::Ordinary
        ));
        assert!(!lk(&c.inv).contains_key(&39));
        assert_eq!(*lk(&c.hp), 1.0);
    }

    #[test]
    fn vanilla_combat2_totem_void_and_kill_bypass() {
        let _guard = crate::world_guard();
        let _g = LaneGuard::new();
        for kind in [TotemDamageKind::Void, TotemDamageKind::Kill] {
            let (c, _r) = client_pair();
            let totem = name_to_item("totem_of_undying").unwrap();
            lk(&c.inv)
                .insert(45, (totem, 1, Vec::new()));
            *lk(&c.hp) = 0.0;
            assert!(
                !try_trigger_totem_of_undying(&c, 0.0, kind),
                "{kind:?} must bypass totem protection"
            );
            assert_eq!(
                lk(&c.inv).get(&45),
                Some(&(totem, 1, Vec::new()))
            );
            assert_eq!(*lk(&c.hp), 0.0);
        }
    }

    #[test]
    fn vanilla_combat2_totem_check_void_zone_no_pop() {
        let _guard = crate::world_guard();
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        let totem = name_to_item("totem_of_undying").unwrap();
        lk(&c.inv)
            .insert(45, (totem, 1, Vec::new()));
        // Below the overworld floor: fellOutOfWorld bypasses protection.
        {
            let mut p = lk(&c.pos);
            p.1 = f64::from(DimId::Overworld.min_y()) - 5.0;
        }
        *lk(&c.hp) = 0.0;
        totem_check(&c);
        assert_eq!(*lk(&c.hp), 0.0);
        assert_eq!(
            lk(&c.inv).get(&45),
            Some(&(totem, 1, Vec::new()))
        );
        // Back inside the world the same lethal check pops the totem.
        {
            let mut p = lk(&c.pos);
            p.1 = 70.0;
        }
        totem_check(&c);
        assert_eq!(*lk(&c.hp), 1.0);
    }

    #[test]
    fn vanilla_combat2_totem_check_generic_kill_bypasses_until_respawn_reset() {
        let _guard = crate::world_guard();
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        let totem = name_to_item("totem_of_undying").unwrap();
        lk(&c.inv)
            .insert(45, (totem, 1, Vec::new()));
        c.generic_kill_pending.store(true, Ordering::SeqCst);
        *lk(&c.hp) = 0.0;
        totem_check(&c);
        totem_check(&c);
        assert_eq!(*lk(&c.hp), 0.0);
        assert_eq!(
            lk(&c.inv).get(&45),
            Some(&(totem, 1, Vec::new()))
        );
        assert!(c.generic_kill_pending.load(Ordering::SeqCst));
        reset_player_food(&c);
        assert!(!c.generic_kill_pending.load(Ordering::SeqCst));
    }

    #[test]
    fn vanilla_combat2_totem_nonlethal_and_empty_hand() {
        let _guard = crate::world_guard();
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        let totem = name_to_item("totem_of_undying").unwrap();
        lk(&c.inv)
            .insert(45, (totem, 1, Vec::new()));
        // Survived hit: hp still positive -> no pop.
        assert!(!try_trigger_totem_of_undying(&c, 0.5, TotemDamageKind::Ordinary));
        assert_eq!(
            lk(&c.inv).get(&45),
            Some(&(totem, 1, Vec::new()))
        );
        // Lethal but no totem anywhere: still no pop.
        lk(&c.inv).clear();
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
        let _guard = crate::world_guard();
        let _g = LaneGuard::new();
        let (c, _r) = client_pair();
        {
            let mut p = lk(&c.pos);
            p.0 = 16.0;
            p.1 = 70.0;
            p.2 = 10.0;
        }
        let c = Arc::new(c);
        lk(clients())
            .push(c.clone());
        lk(region())
            .hot
            .insert((1, 0));
        let mut comps = Vec::new();
        comp_set(
            &mut comps,
            COMP_ENCHANTMENTS,
            enchantments_data(&[(19, 2)], true),
        );
        lk(arrows()).push(Arrow {
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
        lk(trident_returning())
            .insert(99901);
        let before = {
            let ar = lk(arrows());
            let a = ar.iter().find(|a| a.eid == 99901).unwrap();
            ((a.x - 16.0).powi(2) + (a.y - 70.9).powi(2)).sqrt()
        };
        arrow_tick();
        let after = {
            let ar = lk(arrows());
            let a = ar.iter().find(|a| a.eid == 99901).unwrap();
            ((a.x - 16.0).powi(2) + (a.y - 70.9).powi(2)).sqrt()
        };
        assert!(
            after < before,
            "loyalty trident must close on its owner ({before} -> {after})"
        );
        // Drop it on the owner's eye: next tick lands it as a pickup there.
        {
            let mut ar = lk(arrows());
            let a = ar.iter_mut().find(|a| a.eid == 99901).unwrap();
            a.x = 16.0;
            a.y = 70.9;
            a.z = 10.0;
        }
        arrow_tick();
        assert!(lk(arrows())
            .iter()
            .all(|a| a.eid != 99901));
        let drop_at_owner = lk(drops())
            .iter()
            .any(|d| d.item == 9999 && (d.x - 16.0).abs() < 1.0);
        assert!(drop_at_owner, "returned trident lands at the owner's feet");
        lk(region())
            .hot
            .remove(&(1, 0));
    }

    #[test]
    fn vanilla_combat2_riptide_spin_state() {
        let _guard = crate::world_guard();
        let _g = LaneGuard::new();
        TICK_IDX.store(1000, Ordering::Relaxed);
        lk(riptide_spin())
            .insert(4242, 1000 + server_ticks(20));
        assert_eq!(spin_attack_ticks_left(4242), 20);
        TICK_IDX.store(1019, Ordering::Relaxed);
        assert_eq!(spin_attack_ticks_left(4242), 1);
        TICK_IDX.store(1020, Ordering::Relaxed);
        assert_eq!(spin_attack_ticks_left(4242), 0);
    }

    #[test]
    fn parity_live_wire_wind_totem_trident() {
        let _guard = world_guard();
        crate::test_support::require_vanilla_data();
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
        lk(mobs()).push(charge.clone());
        lane.set_block(DimId::Overworld, 33_000, 81, 33_000,
            reg.state_of("minecraft:stone", None));
        combat2_state_tick();
        assert_eq!(*lk(&charge.hp), 0.0,
            "solid contact bursts the charge through wind_charge_burst");
        lk(mobs())
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
        lk(mobs()).push(mob.clone());
        *lk(&mob.fire_ticks) = 80;
        let cauldron = reg.with_prop(
            reg.state_of("minecraft:water_cauldron", None), "level", "2");
        lane.set_block(DimId::Overworld, 33_010, 81, 33_000, cauldron);
        mob_hurt_window_tick();
        assert_eq!(*lk(&mob.fire_ticks), 0,
            "water cauldron quenches a burning entity");
        assert_eq!(
            reg.get_prop(block_at_dim(DimId::Overworld, 33_010, 81, 33_000), "level").as_deref(),
            Some("1"),
            "extinguishing drops one cauldron level",
        );
        lk(mobs()).retain(|m| m.eid != mob.eid);

        // Totem: the live damage path's totem check keeps the holder alive.
        let (client, _rx) = client_pair();
        lk(&client.inv)
            .insert(45, (1163, 1, Vec::new()));
        assert!(try_trigger_totem_of_undying(&client, -1.0, TotemDamageKind::Ordinary));
        assert_eq!(*lk(&client.hp), 1.0);

        // Trident: a returning loyalty trident closes on its owner through
        // `arrow_tick`'s live `trident_returning` pass.
        let (owner, _r2) = client_pair();
        {
            let mut p = lk(&owner.pos);
            p.0 = 16.0;
            p.1 = 70.0;
            p.2 = 10.0;
        }
        lk(clients()).push(Arc::new(owner));
        lk(region()).hot.insert((1, 0));
        let mut comps = Vec::new();
        comp_set(&mut comps, COMP_ENCHANTMENTS, enchantments_data(&[(19, 2)], true));
        lk(arrows()).push(Arrow {
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
        lk(trident_returning()).insert(99901);
        let before = {
            let ar = lk(arrows());
            let a = ar.iter().find(|a| a.eid == 99901).unwrap();
            ((a.x - 16.0).powi(2) + (a.y - 70.9).powi(2)).sqrt()
        };
        arrow_tick();
        let after = {
            let ar = lk(arrows());
            let a = ar.iter().find(|a| a.eid == 99901).unwrap();
            ((a.x - 16.0).powi(2) + (a.y - 70.9).powi(2)).sqrt()
        };
        assert!(after < before, "loyalty trident returns through arrow_tick");
    }
}

/// MEGA-SCALE T1 + T3 fixtures: a large power-aware blast slices into bounded
/// per-tick rings, invalid input fails closed without a mutation, and a
/// 2,000-strong primed-TNT wave ticks in one pass.
#[cfg(all(test, megu_vanilla_data))]
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
        pub(super) fn take() -> Self {
            let tile_drops = GR_DO_TILE_DROPS.swap(1, Ordering::Relaxed);
            Self {
                world: world().take_all(),
                primed: std::mem::take(&mut *lk(primed())),
                drops: std::mem::take(&mut *lk(drops())),
                drop_pending: std::mem::take(
                    &mut *lk(drop_pending()),
                ),
                xps: std::mem::take(&mut *lk(xps())),
                xp_pending: std::mem::take(
                    &mut *lk(xp_pending()),
                ),
                queue: std::mem::take(
                    &mut *lk(mega_blast_queue()),
                ),
                clients: std::mem::take(&mut *lk(clients())),
                forced: std::mem::take(
                    &mut *lk(forceloaded()),
                ),
                region: test_region(),
                rng: world_rng::saved_state(),
                tile_drops,
                scheduled: std::mem::replace(
                    &mut *lk(block_ticks()),
                    scheduled_ticks::ScheduledTicks::new(),
                ),
            }
        }
    }

    impl std::ops::Drop for MegaWorld {
        fn drop(&mut self) {
            world().restore_all(std::mem::take(&mut self.world));
            *lk(primed()) = std::mem::take(&mut self.primed);
            *lk(drops()) = std::mem::take(&mut self.drops);
            *lk(drop_pending()) =
                std::mem::take(&mut self.drop_pending);
            *lk(xps()) = std::mem::take(&mut self.xps);
            *lk(xp_pending()) =
                std::mem::take(&mut self.xp_pending);
            *lk(mega_blast_queue()) =
                std::mem::take(&mut self.queue);
            *lk(clients()) = std::mem::take(&mut self.clients);
            *lk(forceloaded()) =
                std::mem::take(&mut self.forced);
            set_test_region(self.region.0.clone(), self.region.1.clone());
            world_rng::restore(self.rng);
            GR_DO_TILE_DROPS.store(self.tile_drops, Ordering::Relaxed);
            *lk(block_ticks()) = std::mem::replace(
                &mut self.scheduled,
                scheduled_ticks::ScheduledTicks::new(),
            );
        }
    }

    /// Write `st` into every cell of the `(2*half+1)^3` cube centred on `at`,
    /// bypassing `set_block` so explicit entries — including air — stay in the
    /// overlay and `block_at` never evaluates worldgen inside the blast bounds.
    fn fill_cube(at: (i32, i32, i32), half: i32, st: u32) {
        let mut w = world().write_all();
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
        let w = world().read_all();
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
            !lk(mega_blast_queue())
                .is_empty(),
            "a >{MEGA_BLAST_MAX_PER_TICK}-cell blast must leave queued rings"
        );
        for _ in 0..128 {
            if lk(mega_blast_queue())
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
            lk(mega_blast_queue())
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
        let dp = lk(drops());
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
                lk(mega_blast_queue())
                    .is_empty(),
                "{label} queued a ring plan"
            );
            assert!(
                lk(drops())
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
            let mut pl = lk(primed());
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
            let pl = lk(primed());
            assert_eq!(pl.len(), N as usize, "a fuse of {expected} must not detonate early");
            assert!(
                pl.iter().all(|p| p.left == expected),
                "every fuse must decrement exactly once per tick"
            );
        }
        primed_tick();
        let pl = lk(primed());
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

// ---------- per-entity dimension tests (H1) ----------
// The entity-dim side table, the dim-scoped XP/merge paths and the
// dim-threaded blast/arrow/firework ticks. Pure side-table and merge tests
// need no registry; world-touching ones snapshot the stores they use.
#[cfg(all(test, megu_vanilla_data))]
mod dim_entity_tests {
    use super::effect_tests::client_pair;
    use super::*;

    /// Save/restore every shared store a dim-scoped entity test can touch.
    /// Restored on `Drop`, so a failed assertion still leaves the next test a
    /// clean world.
    pub(super) struct DimWorld {
        world: HashMap<DimChunkKey, HashMap<u32, u32>>,
        mobs: Vec<Arc<Mob>>,
        live_mob_states: HashMap<i32, LiveMobState>,
        arrows: Vec<Arrow>,
        primed: Vec<Primed>,
        fireworks: Vec<Firework>,
        drops: Vec<Drop>,
        drop_pending: HashSet<i32>,
        xps: Vec<Xp>,
        xp_pending: HashSet<i32>,
        entity_dims: HashMap<i32, DimId>,
        queue: std::collections::VecDeque<MegaBlastJob>,
        clients: Vec<Arc<Client>>,
        forced: HashSet<(i32, i32)>,
        region: RegionSets,
        rng: u64,
        tile_drops: u8,
        mob_loot: u8,
        scheduled: scheduled_ticks::ScheduledTicks,
        fire_ticks: HashMap<i32, u32>,
        sheared: HashSet<i32>,
        tick: u64,
        thundering: u64,
    }

    impl DimWorld {
        pub(super) fn take() -> Self {
            let tile_drops = GR_DO_TILE_DROPS.swap(1, Ordering::Relaxed);
            let mob_loot = GR_MOB_LOOT.swap(1, Ordering::Relaxed);
            Self {
                world: world().take_all(),
                mobs: std::mem::take(&mut *lk(mobs())),
                live_mob_states: std::mem::take(
                    &mut *lk(live_mob_states()),
                ),
                arrows: std::mem::take(&mut *lk(arrows())),
                primed: std::mem::take(&mut *lk(primed())),
                fireworks: std::mem::take(
                    &mut *lk(fireworks()),
                ),
                drops: std::mem::take(&mut *lk(drops())),
                drop_pending: std::mem::take(
                    &mut *lk(drop_pending()),
                ),
                xps: std::mem::take(&mut *lk(xps())),
                xp_pending: std::mem::take(
                    &mut *lk(xp_pending()),
                ),
                entity_dims: std::mem::take(
                    &mut *wr(entity_dims()),
                ),
                queue: std::mem::take(
                    &mut *lk(mega_blast_queue()),
                ),
                clients: std::mem::take(&mut *lk(clients())),
                forced: std::mem::take(
                    &mut *lk(forceloaded()),
                ),
                region: test_region(),
                rng: world_rng::saved_state(),
                tile_drops,
                mob_loot,
                scheduled: std::mem::replace(
                    &mut *lk(block_ticks()),
                    scheduled_ticks::ScheduledTicks::new(),
                ),
                fire_ticks: std::mem::take(
                    &mut *lk(mob_fire_ticks()),
                ),
                sheared: std::mem::take(
                    &mut *lk(sheared()),
                ),
                tick: TICK_IDX.swap(0, Ordering::Relaxed),
                thundering: THUNDERING.swap(0, Ordering::Relaxed),
            }
        }
    }

    impl std::ops::Drop for DimWorld {
        fn drop(&mut self) {
            world().restore_all(std::mem::take(&mut self.world));
            *lk(mobs()) = std::mem::take(&mut self.mobs);
            *lk(live_mob_states()) =
                std::mem::take(&mut self.live_mob_states);
            *lk(arrows()) = std::mem::take(&mut self.arrows);
            *lk(primed()) = std::mem::take(&mut self.primed);
            *lk(fireworks()) =
                std::mem::take(&mut self.fireworks);
            *lk(drops()) = std::mem::take(&mut self.drops);
            *lk(drop_pending()) =
                std::mem::take(&mut self.drop_pending);
            *lk(xps()) = std::mem::take(&mut self.xps);
            *lk(xp_pending()) =
                std::mem::take(&mut self.xp_pending);
            *wr(entity_dims()) =
                std::mem::take(&mut self.entity_dims);
            *lk(mega_blast_queue()) =
                std::mem::take(&mut self.queue);
            *lk(clients()) = std::mem::take(&mut self.clients);
            *lk(forceloaded()) =
                std::mem::take(&mut self.forced);
            set_test_region(self.region.0.clone(), self.region.1.clone());
            world_rng::restore(self.rng);
            GR_DO_TILE_DROPS.store(self.tile_drops, Ordering::Relaxed);
            GR_MOB_LOOT.store(self.mob_loot, Ordering::Relaxed);
            *lk(block_ticks()) = std::mem::replace(
                &mut self.scheduled,
                scheduled_ticks::ScheduledTicks::new(),
            );
            *lk(mob_fire_ticks()) =
                std::mem::take(&mut self.fire_ticks);
            *lk(sheared()) = std::mem::take(&mut self.sheared);
            TICK_IDX.store(self.tick, Ordering::Relaxed);
            THUNDERING.store(self.thundering, Ordering::Relaxed);
        }
    }

    fn mob_in(eid: i32, pos: (f64, f64, f64), dim: DimId, name: &str) -> Arc<Mob> {
        let past = Instant::now() - Duration::from_secs(10);
        Arc::new(Mob {
            dim,
            eid,
            uuid: [0; 16],
            etype: name_to_etype(name).unwrap(),
            pos: Mutex::new((pos.0, pos.1, pos.2, 0.0, 0.0)),
            hp: Mutex::new(20.0),
            last_hit: Mutex::new(past),
            hurt_at: Mutex::new(past),
            last_hurt_tick: Mutex::new(None),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            vel: Mutex::new([0.0; 3]),
        })
    }

    fn arrow_at(eid: i32, x: f64, y: f64, z: f64, dx: f64) -> Arrow {
        Arrow {
            eid,
            owner: 7,
            x,
            y,
            z,
            dx,
            dy: 0.0,
            dz: 0.0,
            ttl: 100,
            dmg: 5.0,
            fire: 0,
            punch: 0,
            kind: 0,
            loy: 0,
            ret_item: 0,
            ret_comps: Vec::new(),
            pierce: 0,
            last_hit: -1,
            stuck: false,
        }
    }

    /// `fill_cube` over one dimension's overlay: `block_at_dim` inside the box
    /// reads only the explicit states, never generated terrain.
    fn fill_cube_dim(dim: DimId, at: (i32, i32, i32), half: i32, st: u32) {
        let mut w = world().write_all();
        for x in at.0 - half..=at.0 + half {
            for z in at.2 - half..=at.2 + half {
                let c = w.entry((dim, x >> 4, z >> 4)).or_default();
                for y in at.1 - half..=at.1 + half {
                    c.insert(local_idx(x, y, z), st);
                }
            }
        }
    }

    fn count_in_cube_dim(dim: DimId, at: (i32, i32, i32), half: i32, st: u32) -> usize {
        let w = world().read_all();
        let mut n = 0usize;
        for x in at.0 - half..=at.0 + half {
            for z in at.2 - half..=at.2 + half {
                let Some(c) = w.get(&(dim, x >> 4, z >> 4)) else {
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

    #[test]
    fn kill_mob_spawns_xp_in_the_mobs_own_dimension() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = DimWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        let etype = name_to_etype("zombie").unwrap();
        let mob = mob_in(88_001, (8.0, 40.0, 8.0), DimId::Nether, "zombie");
        lk(mobs()).push(mob);
        kill_mob(88_001, etype, 8.0, 40.0, 8.0);
        let xs = lk(xps());
        assert_eq!(xs.len(), 1, "one XP orb for the killed zombie");
        assert_eq!(
            entity_dim_of(xs[0].eid),
            DimId::Nether,
            "the orb must land in the mob's dimension, not the Overworld"
        );
        assert_eq!(xs[0].val, mob_xp(etype));
    }

    #[test]
    fn mob_env_die_drops_in_the_mobs_own_dimension() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = DimWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        let etype = name_to_etype("zombie").unwrap();
        lk(mobs()).push(mob_in(
            88_002,
            (8.0, 40.0, 8.0),
            DimId::from_raw(3),
            "zombie",
        ));
        mob_env_die(88_002, etype, 8.0, 40.0, 8.0, false);
        // Environmental death: drops but no XP.
        assert!(lk(xps()).is_empty());
        // The scatter probe ran against the mob's dimension: with no datapack
        // generator the cells are air, so the drop lands near the death point.
        let dl = lk(drops());
        assert!(!dl.is_empty(), "mob loot still drops in a custom dimension");
        for d in dl.iter() {
            assert!(
                (d.x - 8.0).abs() < 2.0 && (d.y - 40.0).abs() < 2.0 && (d.z - 8.0).abs() < 2.0,
                "scatter in the entity's own dimension stays near the death point, got ({},{},{})",
                d.x,
                d.y,
                d.z
            );
        }
    }

    #[test]
    fn explode_at_dim_destroys_only_its_own_dimensions_blocks() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = DimWorld::take();
        world_rng::seed(0x05ee_dd1e);
        let reg = breg().unwrap();
        let mushroom = reg.state_of("minecraft:red_mushroom", None);
        let at = (500_032, 100, -500_032);
        let half = (4.0f64 * 1.75).ceil() as i32 + 2;
        // Identical fill in both dimensions; the blast must clear exactly one.
        fill_cube_dim(DimId::Nether, at, half, mushroom);
        fill_cube_dim(DimId::Overworld, at, half, mushroom);
        let ow_before = count_in_cube_dim(DimId::Overworld, at, half, mushroom);
        let nether_before = count_in_cube_dim(DimId::Nether, at, half, mushroom);
        explode_at_dim(DimId::Nether, at.0 as f64 + 0.5, at.1 as f64, at.2 as f64 + 0.5, 4.0);
        for _ in 0..64 {
            if lk(mega_blast_queue())
                .is_empty()
            {
                break;
            }
            mega_blast_tick_drain();
        }
        let nether_after = count_in_cube_dim(DimId::Nether, at, half, mushroom);
        assert!(
            nether_after < nether_before,
            "the Nether blast must destroy Nether cells ({nether_before} -> {nether_after})"
        );
        assert_eq!(
            count_in_cube_dim(DimId::Overworld, at, half, mushroom),
            ow_before,
            "the Overworld cells at the same coordinates are untouched"
        );
    }

    #[test]
    fn primed_tnt_detonates_in_its_registered_dimension() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = DimWorld::take();
        world_rng::seed(0x05ee_d7a7);
        let reg = breg().unwrap();
        let mushroom = reg.state_of("minecraft:red_mushroom", None);
        let at = (8, 60, 8);
        let half = (4.0f64 * 1.75).ceil() as i32 + 2;
        fill_cube_dim(DimId::Nether, at, half, mushroom);
        fill_cube_dim(DimId::Overworld, at, half, mushroom);
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        lk(primed()).push(Primed {
            eid: 88_010,
            x: 8.5,
            y: 60.5,
            z: 8.5,
            left: 1,
        });
        set_entity_dim(88_010, DimId::Nether);
        let ow_before = count_in_cube_dim(DimId::Overworld, at, half, mushroom);
        let nether_before = count_in_cube_dim(DimId::Nether, at, half, mushroom);
        primed_tick();
        assert!(lk(primed()).is_empty());
        assert!(
            count_in_cube_dim(DimId::Nether, at, half, mushroom) < nether_before,
            "the fuse must detonate in the primed entity's own dimension"
        );
        assert_eq!(
            count_in_cube_dim(DimId::Overworld, at, half, mushroom),
            ow_before
        );
        assert_eq!(
            entity_dim_of(88_010),
            DimId::Overworld,
            "the detonated entity's dim entry is retired"
        );
    }

    #[test]
    fn projectile_nearest_player_and_loyalty_mob_are_resolved_before_wall() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = DimWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        let dim = DimId::Overworld;
        let reg = breg().unwrap();
        for (x, name) in [(1, "water"), (2, "short_grass"), (8, "stone")] {
            set_block_dim(
                dim,
                x,
                200,
                0,
                reg.state_of(&format!("minecraft:{name}"), None),
            );
        }
        let mob = mob_in(890_001, (6., 200., 0.5), dim, "zombie");
        lk(mobs()).push(mob.clone());
        let (player, _rx) = client_pair();
        let player = Arc::new(player);
        *lk(&player.pos) = (4., 200., 0.5, 0., 0., true);
        lk(clients()).push(player.clone());
        lk(arrows()).push(arrow_at(890_002, 0.5, 200.5, 0.5, 8.));
        arrow_tick();
        assert_eq!(*lk(&mob.hp), 20.);
        let hp = *lk(&player.hp);
        assert!(hp < 20.);
        *lk(&player.invulnerable) = 0;
        *lk(&player.pos) = (7.5, 200., 0.5, 0., 0., true);
        let mut trident = arrow_at(890_003, 0.5, 200.5, 0.5, 8.);
        trident.kind = 1;
        trident.loy = 1;
        lk(arrows()).push(trident);
        arrow_tick();
        assert_eq!(*lk(&mob.hp), 15.);
        assert_eq!(*lk(&player.hp), hp);
        assert!(lk(trident_returning()).remove(&890_003));
        lk(trident_seen()).remove(&890_003);
    }

    #[test]
    fn arrow_sticks_into_blocks_in_its_own_dimension() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = DimWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        let reg = breg().unwrap();
        let stone = reg.state_of("minecraft:stone", None);
        // Nether stone where both arrows pass: only the Nether arrow sticks.
        {
            let mut w = world().write_all();
            w.entry((DimId::Nether, 0, 0))
                .or_default()
                .insert(local_idx(8, 100, 8), stone);
        }
        lk(arrows()).extend([
            arrow_at(88_020, 8.5, 100.5, 8.5, 0.0),
            arrow_at(88_021, 8.5, 100.5, 8.5, 0.0),
        ]);
        set_entity_dim(88_020, DimId::Nether);
        arrow_tick();
        let ar = lk(arrows());
        let nether_arrow = ar.iter().find(|a| a.eid == 88_020).unwrap();
        let ow_arrow = ar.iter().find(|a| a.eid == 88_021).unwrap();
        assert!(nether_arrow.stuck, "a Nether arrow sticks into a Nether block");
        assert!(
            !ow_arrow.stuck,
            "a Nether block must not stop an Overworld arrow"
        );
        drop(ar);
        assert_eq!(entity_dim_of(88_021), DimId::Overworld);
    }

    #[test]
    fn arrow_hits_only_mobs_in_its_own_dimension() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = DimWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        // The Overworld mob is listed first so the skip is observed, then the
        // Nether mob stands in the same spot and takes the hit.
        let om = mob_in(88_030, (9.0, 100.0, 8.5), DimId::Overworld, "zombie");
        let nm = mob_in(88_031, (9.0, 100.0, 8.5), DimId::Nether, "zombie");
        lk(mobs()).extend([om, nm]);
        lk(arrows())
            .push(arrow_at(88_022, 8.0, 100.0, 8.5, 2.0));
        set_entity_dim(88_022, DimId::Nether);
        arrow_tick();
        let ml = lk(mobs());
        let ow_hp = *lk(&ml.iter().find(|m| m.eid == 88_030).unwrap().hp);
        let nether_hp = *lk(&ml.iter().find(|m| m.eid == 88_031).unwrap().hp);
        assert_eq!(ow_hp, 20.0, "a Nether arrow passes through an Overworld mob");
        assert!(nether_hp < 20.0, "a Nether arrow damages a Nether mob");
    }

    #[test]
    fn firework_blast_damages_only_its_own_dimension() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = DimWorld::take();
        let om = mob_in(88_040, (9.0, 100.0, 8.5), DimId::Overworld, "zombie");
        let nm = mob_in(88_041, (9.0, 100.0, 8.5), DimId::Nether, "zombie");
        lk(mobs()).extend([om, nm]);
        firework_explode(DimId::Nether, 9.0, 100.5, 8.5, 2, -1);
        let ml = lk(mobs());
        let ow_hp = *lk(&ml.iter().find(|m| m.eid == 88_040).unwrap().hp);
        let nether_hp = *lk(&ml.iter().find(|m| m.eid == 88_041).unwrap().hp);
        assert_eq!(ow_hp, 20.0);
        assert!(nether_hp < 20.0, "the burst must hit same-dimension mobs");
        drop(ml);
        // The tick path resolves the dim off the side table and retires it.
        lk(fireworks())
            .push(Firework {
                eid: 88_042,
                owner: 7,
                x: 9.0,
                y: 100.5,
                z: 8.5,
                dx: 0.0,
                dy: 0.0,
                dz: 0.0,
                ttl: 1,
                explosions: 0,
            });
        set_entity_dim(88_042, DimId::from_raw(3));
        fireworks_step();
        assert!(lk(fireworks()).is_empty());
        assert_eq!(entity_dim_of(88_042), DimId::Overworld);
    }

    #[test]
    fn explosion_packets_reach_only_same_dimension_clients() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = DimWorld::take();
        set_test_region(HashSet::from([(0, 0)]), HashSet::new());
        // Two clients at the same spot, one in the Nether one in the
        // Overworld: only the Nether client takes blast damage.
        let (mut nether_client, _rx1) = client_pair();
        nether_client.eid = 4242;
        *lk(&nether_client.dim) = DimId::Nether;
        *lk(&nether_client.pos) =
            (9.0, 100.0, 8.5, 0.0, 0.0, true);
        let (mut ow_client, _rx2) = client_pair();
        ow_client.eid = 4343;
        *lk(&ow_client.pos) =
            (9.0, 100.0, 8.5, 0.0, 0.0, true);
        let nether_client = Arc::new(nether_client);
        let ow_client = Arc::new(ow_client);
        lk(clients())
            .extend([nether_client.clone(), ow_client.clone()]);
        explode_at_dim(DimId::Nether, 9.0, 100.5, 8.5, 4.0);
        let nether_hp = *lk(&nether_client.hp);
        let ow_hp = *lk(&ow_client.hp);
        assert!(nether_hp < 20.0, "a Nether blast hurts a Nether player");
        assert_eq!(ow_hp, 20.0, "a Nether blast cannot hurt an Overworld player");
    }
}


// ---------- explosion parity regressions ----------
#[cfg(test)]
mod explosion_parity_tests {
    #[cfg(megu_vanilla_data)]
    use super::dim_entity_tests::DimWorld;
    use super::*;

    #[test]
    fn explosion_damage_point_blank_tnt_is_vanilla_57() {
        assert_eq!(explosion_damage_value(4.0, 0.0, 1.0), 57.0);
        assert_eq!(explosion_damage_value(4.0, 0.0, 0.0), 0.0);
    }

    #[test]
    fn explosion_knockback_is_reduced_by_resistance_attribute() {
        let unarmored = explosion_knockback_strength(0.5, 1.0, 0.0);
        let blast_protection_iv = explosion_knockback_strength(0.5, 1.0, 0.6);
        assert!((unarmored - 0.5).abs() < 1e-9);
        assert!((blast_protection_iv - 0.2).abs() < 1e-9);
        assert_eq!(explosion_knockback_strength(0.5, 1.0, 1.0), 0.0);
    }

    #[cfg(megu_vanilla_data)]
    #[test]
    fn explosion_seen_percent_open_air_one_and_solid_wall_zero() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = DimWorld::take();
        let reg = breg().unwrap();
        let stone = reg.state_of("minecraft:stone", None);
        let center = [0.5, 200.5, 0.5];
        let target = player_explosion_aabb(5.5, 200.0, 0.5);
        assert_eq!(seen_percent(center, target, DimId::Overworld), 1.0);
        for y in 198..=202 {
            for z in -2..=3 {
                set_block_dim(DimId::Overworld, 3, y, z, stone);
            }
        }
        assert_eq!(seen_percent(center, target, DimId::Overworld), 0.0);
    }

    #[cfg(megu_vanilla_data)]
    #[test]
    fn explosion_live_mob_blast_never_breaks_obsidian_or_ancient_debris() {
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _w = DimWorld::take();
        world_rng::seed(0x0b5e_1a57);
        let reg = breg().unwrap();
        let obsidian = reg.state_of("minecraft:obsidian", None);
        let debris = reg.state_of("minecraft:ancient_debris", None);
        let frame = reg.state_of("minecraft:end_portal_frame", None);
        let dirt = reg.state_of("minecraft:dirt", None);
        let at = (1000, 100, 1000);
        set_block_dim(DimId::Overworld, at.0, at.1, at.2, dirt);
        set_block_dim(DimId::Overworld, at.0 + 1, at.1, at.2, obsidian);
        set_block_dim(DimId::Overworld, at.0 - 1, at.1, at.2, debris);
        set_block_dim(DimId::Overworld, at.0, at.1, at.2 + 1, frame);
        GR_MOB_GRIEFING.store(1, Ordering::Relaxed);
        live_mob_explosion(
            DimId::Overworld,
            [at.0 as f64 + 0.5, at.1 as f64 + 0.5, at.2 as f64 + 0.5],
            4.0,
            &[],
        );
        assert_eq!(block_at_dim(DimId::Overworld, at.0, at.1, at.2), 0);
        assert_eq!(block_at_dim(DimId::Overworld, at.0 + 1, at.1, at.2), obsidian);
        assert_eq!(block_at_dim(DimId::Overworld, at.0 - 1, at.1, at.2), debris);
        assert_eq!(block_at_dim(DimId::Overworld, at.0, at.1, at.2 + 1), frame);
        // `mobGriefing=false` suppresses block removal entirely while the
        // same raycast explosion still emits and applies entity effects.
        set_block_dim(DimId::Overworld, at.0, at.1, at.2, dirt);
        GR_MOB_GRIEFING.store(0, Ordering::Relaxed);
        live_mob_explosion(
            DimId::Overworld,
            [at.0 as f64 + 0.5, at.1 as f64 + 0.5, at.2 as f64 + 0.5],
            4.0,
            &[],
        );
        assert_eq!(block_at_dim(DimId::Overworld, at.0, at.1, at.2), dirt);
        GR_MOB_GRIEFING.store(1, Ordering::Relaxed);
    }
}

// ---------- per-entity dimension tests, no-registry lane (H1) ----------
// The side table and both merge partitions run without the vanilla fixture
// set, so these tests also run on strict-public exports that gate the
// `megu_vanilla_data` modules out.
#[cfg(test)]
mod dim_entity_pure_tests {
    use super::*;

    /// Save/restore the orb pool, the merge-pending marks and the eid->DimId
    /// table so each test starts from empty stores even after a failed
    /// assertion.
    struct OrbStores {
        xps: Vec<Xp>,
        xp_pending: HashSet<i32>,
        entity_dims: HashMap<i32, DimId>,
    }

    impl OrbStores {
        fn take() -> Self {
            Self {
                xps: std::mem::take(&mut *lk(xps())),
                xp_pending: std::mem::take(
                    &mut *lk(xp_pending()),
                ),
                entity_dims: std::mem::take(
                    &mut *wr(entity_dims()),
                ),
            }
        }
    }

    impl std::ops::Drop for OrbStores {
        fn drop(&mut self) {
            *lk(xps()) = std::mem::take(&mut self.xps);
            *lk(xp_pending()) =
                std::mem::take(&mut self.xp_pending);
            *wr(entity_dims()) =
                std::mem::take(&mut self.entity_dims);
        }
    }

    #[test]
    fn entity_dim_side_table_defaults_sets_and_clears() {
        let _guard = world_guard();
        let _s = OrbStores::take();
        assert_eq!(entity_dim_of(9_999_001), DimId::Overworld);
        set_entity_dim(9_999_001, DimId::Nether);
        assert_eq!(entity_dim_of(9_999_001), DimId::Nether);
        set_entity_dim(9_999_001, DimId::from_raw(3));
        assert_eq!(entity_dim_of(9_999_001), DimId::from_raw(3));
        clear_entity_dim(9_999_001);
        assert_eq!(entity_dim_of(9_999_001), DimId::Overworld);
        // Registering the Overworld stores no row — it is already the default.
        set_entity_dim(9_999_002, DimId::Nether);
        set_entity_dim(9_999_002, DimId::Overworld);
        assert!(rd(entity_dims())
            .get(&9_999_002)
            .is_none());
    }

    #[test]
    fn xp_spawn_dim_tags_the_orbs_dimension() {
        let _guard = world_guard();
        let _s = OrbStores::take();
        let eid = xp_spawn_dim(DimId::Nether, 1.0, 60.0, 1.0, 7);
        assert_eq!(entity_dim_of(eid), DimId::Nether);
        let eid = xp_spawn_dim(DimId::from_raw(3), 1.0, 60.0, 1.0, 7);
        assert_eq!(entity_dim_of(eid), DimId::from_raw(3));
        // The compatibility wrapper keeps landing orbs in the Overworld.
        let eid = xp_spawn(1.0, 60.0, 1.0, 7);
        assert_eq!(entity_dim_of(eid), DimId::Overworld);
        assert!(rd(entity_dims())
            .get(&eid)
            .is_none());
    }

    #[test]
    fn xp_pending_merge_partitions_by_dimension() {
        let _guard = world_guard();
        let _s = OrbStores::take();
        // Coincident donors in two dimensions: neither may absorb the other.
        xp_spawn_dim(DimId::Nether, 4.0, 60.0, 4.0, 10);
        let b = xp_spawn_dim(DimId::from_raw(3), 4.0, 60.0, 4.0, 10);
        let eaten = {
            let mut xs = std::mem::take(&mut *lk(xps()));
            let pending = lk(xp_pending()).clone();
            let eaten = xp_merge_pending_locked(&mut xs, &pending);
            *lk(xps()) = xs;
            eaten
        };
        assert!(eaten.is_empty(), "cross-dimension orbs must never merge");
        let xs = lk(xps());
        assert_eq!(xs.len(), 2);
        assert_eq!(xs.iter().map(|x| x.val as i64).sum::<i64>(), 20);
        drop(xs);
        // Same-dimension donors still collapse into one orb.
        clear_entity_dim(b);
        set_entity_dim(b, DimId::Nether);
        let eaten = {
            let mut xs = std::mem::take(&mut *lk(xps()));
            let pending = lk(xp_pending()).clone();
            let eaten = xp_merge_pending_locked(&mut xs, &pending);
            *lk(xps()) = xs;
            eaten
        };
        assert_eq!(eaten.len(), 1, "same-dimension donors merge");
        let xs = lk(xps());
        assert_eq!(xs.len(), 1);
        assert_eq!(xs[0].val, 20);
        assert_eq!(entity_dim_of(xs[0].eid), DimId::Nether);
    }

    #[test]
    fn env_merge_partitions_by_dimension() {
        let _guard = world_guard();
        let _s = OrbStores::take();
        let mut xs = vec![
            Xp { eid: 1, x: 4.0, y: 60.0, z: 4.0, val: 10, age: 0 },
            Xp { eid: 2, x: 4.0, y: 60.0, z: 4.0, val: 10, age: 0 },
        ];
        set_entity_dim(2, DimId::Nether);
        assert!(
            merge_xp_orbs(&mut xs).is_empty(),
            "coincident orbs in different dimensions must not merge"
        );
        // Same dimension: the earlier orb absorbs the later one; the eaten
        // slot keeps its stale value until the caller retains it out, so the
        // assertion reads the reported eid and the survivor.
        set_entity_dim(2, DimId::Overworld);
        let merged = merge_xp_orbs(&mut xs);
        assert_eq!(merged, vec![2]);
        assert_eq!(xs[0].val, 20);
    }
}

#[cfg(test)]
mod audit1_mace_tests {
    use super::*;
    use crate::effect_tests::client_pair;

    fn audit_mob(eid: i32, etype: i64, x: f64) -> Arc<Mob> {
        Arc::new(Mob {
            dim: DimId::Overworld,
            eid,
            uuid: [eid as u8; 16],
            etype,
            pos: Mutex::new((x, 66.0, 0.0, 0.0, 0.0)),
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
    fn audit_mace_smash_impulses_mobs_and_nearby_players() {
        let _guard = crate::world_guard();
        let (attacker, _attacker_rx) = client_pair();
        let (mut bystander, mut bystander_rx) = client_pair();
        bystander.eid = 4244;
        bystander.uuid = [44; 16];
        *lk(&bystander.pos) = (1.0, 66.0, 0.0, 0.0, 0.0, false);
        let attacker = Arc::new(attacker);
        let bystander = Arc::new(bystander);
        lk(clients()).extend([attacker.clone(), bystander.clone()]);
        let mob = audit_mob(4300, 124, 1.0);
        let armor_stand = audit_mob(4301, 3, -1.0);
        lk(mobs()).extend([mob.clone(), armor_stand.clone()]);

        wind_burst_apply(&attacker, MaceSmash {
            dim: DimId::Overworld,
            target: 9999,
            center: [0.0, 66.0, 0.0],
            origin: [0.0, 66.0, 0.0],
            level: 1,
            fall: 4.0,
        });

        let vel = *lk(&mob.vel);
        assert!(vel[0] > 1.0, "mob deltaMovement records the radial impulse: {vel:?}");
        assert!(vel[1] > 0.7, "mob impulse adds the Wind Burst launch on top of the smash pulse: {vel:?}");
        let pos = *lk(&mob.pos);
        assert!(pos.0 > 1.0, "mob movement uses the collision sweep instead of a raw teleport: {pos:?}");
        // Vanilla exempts only *marker* armor stands; ours are never markers,
        // so the smash pulse pushes them like any other living entity.
        let stand_vel = *lk(&armor_stand.vel);
        assert!(stand_vel[0] < -1.0, "non-marker armor stand takes the smash impulse: {stand_vel:?}");
        assert!(lk(&armor_stand.pos).0 < -1.0);

        let packet = read_packet(&mut bystander_rx, Instant::now() + Duration::from_secs(1))
            .expect("nearby player receives a velocity impulse packet");
        assert_eq!(packet.0, 0x5a);
        assert_eq!(packet.1, velocity_pkt(4244, 14000, 5600, 0));
        // Smash broadcasts the 0x28 (2013) level event before the Wind Burst pass.
        let smash_event = read_packet(&mut bystander_rx, Instant::now() + Duration::from_secs(1))
            .expect("nearby player receives the mace smash level event");
        assert_eq!(smash_event.0, 0x28);
        // The Wind Burst burst is attacker-centered and includes everyone in
        // range, so the bystander gets a second, explosion-falloff impulse.
        let burst = read_packet(&mut bystander_rx, Instant::now() + Duration::from_secs(1))
            .expect("nearby player receives the Wind Burst knockback packet");
        assert_eq!(burst.0, 0x5a);
        assert_ne!(burst.1, packet.1);

        lk(mobs()).retain(|mob| ![4300, 4301].contains(&mob.eid));
        lk(clients()).retain(|client| ![4242, 4244].contains(&client.eid));
    }

    #[test]
    fn mace_smash_impulse_scales_horizontal_by_knockback_resistance() {
        let center = [0.0, 66.0, 0.0];
        // Vanilla `getKnockbackPower`: (3.5 - dist) * 0.7 * heavy *
        // (1 - resistance); the constant 0.7 vertical is never scaled.
        let near = |resistance, heavy| {
            mace_smash_impulse(center, [1.0, 66.0, 0.0], heavy, resistance)
        };
        let impulse = near(0.0, 1.0).unwrap();
        assert!((impulse[0] - 1.75).abs() < 1e-9 && impulse[1] == 0.7 && impulse[2] == 0.0);
        let impulse = near(0.5, 1.0).unwrap();
        assert!((impulse[0] - 0.875).abs() < 1e-9 && impulse[1] == 0.7);
        let impulse = near(0.0, 2.0).unwrap();
        assert!((impulse[0] - 3.5).abs() < 1e-9);
        assert!(near(1.0, 1.0).is_none());
        assert!(mace_smash_impulse(center, [4.0, 66.0, 0.0], 1.0, 0.0).is_none());
    }

    #[test]
    fn wind_burst_explosion_impulse_uses_radius_falloff_and_level_multiplier() {
        let center = [0.0, 66.0, 0.0];
        // The attacker at the exact center pushes straight up through the
        // eye offset with the full level multiplier.
        let launch =
            wind_burst_explosion_impulse(center, [0.0, 66.0, 0.0], 67.62, 1.2, 0.0).unwrap();
        assert_eq!(launch[0], 0.0);
        assert_eq!(launch[2], 0.0);
        assert!((launch[1] - 1.2).abs() < 1e-9);
        // Inclusion is gated on feet distance <= 3.5.
        assert!(wind_burst_explosion_impulse(center, [3.5, 66.0, 0.0], 66.0, 1.2, 0.0).is_some());
        assert!(wind_burst_explosion_impulse(center, [3.51, 66.0, 0.0], 66.0, 1.2, 0.0).is_none());
        // (1 - d/3.5) falloff, scaled by level and explosion knockback resist.
        let mid = wind_burst_explosion_impulse(center, [1.75, 66.0, 0.0], 66.0, 1.2, 0.0).unwrap();
        assert!((mid[0] - 0.6).abs() < 1e-9);
        let resisted =
            wind_burst_explosion_impulse(center, [1.75, 66.0, 0.0], 66.0, 1.2, 0.5).unwrap();
        assert!((resisted[0] - 0.3).abs() < 1e-9);
    }

    #[test]
    fn sonic_boom_velocity_pushes_target_away_from_the_warden() {
        let _guard = crate::world_guard();
        let (player, _rx) = client_pair();
        let player = Arc::new(player);
        // Warden feet 10 blocks below the player: the push is pure vertical
        // 0.5 * (1 - knockback_resistance) along the eye direction.
        let packet = sonic_boom_velocity(&player, [0.0, 60.0, 0.0], None, 0.0).unwrap();
        assert_eq!(packet, velocity_pkt(player.eid, 0, 4000, 0));
        // Full knockback resistance zeroes the push.
        let resisted = sonic_boom_velocity(&player, [0.0, 60.0, 0.0], None, 1.0).unwrap();
        assert_eq!(resisted, velocity_pkt(player.eid, 0, 0, 0));
    }
}

#[cfg(test)]
mod damage_source_tests {
    use super::*;
    use crate::effect_tests::client_pair;

    #[test]
    fn damage_source_flags_follow_vanilla_tag_sets() {
        let drown = damage_source_flags(&DamageSource::new("drown"));
        assert!(drown.bypasses_armor && drown.bypasses_shield);
        assert!(!drown.is_fire && !drown.is_projectile);

        let fall = damage_source_flags(&DamageSource::new("fall"));
        assert!(fall.is_fall && fall.bypasses_armor);

        let fire = damage_source_flags(&DamageSource::new("in_fire"));
        assert!(fire.is_fire && fire.bypasses_shield && !fire.bypasses_armor);

        let melee = damage_source_flags(&DamageSource::new("mob_attack"));
        assert!(!melee.bypasses_armor && !melee.bypasses_shield && !melee.is_projectile);

        let kill = damage_source_flags(&DamageSource::new("generic_kill"));
        assert!(kill.bypasses_invulnerability && kill.bypasses_armor);
    }

    #[test]
    fn damage_source_totem_kind_gates_only_void_and_kill_sources() {
        let (client, _receiver) = client_pair();
        assert_eq!(
            damage_source_totem_kind(&client, &DamageSource::new("mob_attack")),
            TotemDamageKind::Ordinary
        );
        assert_eq!(
            damage_source_totem_kind(&client, &DamageSource::new("out_of_world")),
            TotemDamageKind::Void
        );
        assert_eq!(
            damage_source_totem_kind(&client, &DamageSource::new("generic_kill")),
            TotemDamageKind::Kill
        );
        client
            .generic_kill_pending
            .store(true, Ordering::SeqCst);
        assert_eq!(
            damage_source_totem_kind(&client, &DamageSource::new("generic")),
            TotemDamageKind::Kill
        );
    }

    #[test]
    fn damage_source_mitigated_skips_armor_only_for_bypassing_sources() {
        let (client, _receiver) = client_pair();
        *lk(&client.armor_pts) = 20;
        let client = Arc::new(client);
        assert_eq!(
            damage_source_mitigated(&client, 10.0, &DamageSource::new("drown")),
            10.0
        );
        let reduced = damage_source_mitigated(&client, 10.0, &DamageSource::new("player_attack"));
        assert!(reduced < 10.0, "armored player_attack must be mitigated: {reduced}");
    }

    #[test]
    fn damage_type_wire_table_maps_registry_names_to_wire_ids() {
        fn registry(entries: &[&str]) -> (i64, Vec<u8>) {
            let mut packet = vec![0x07];
            write_str(&mut packet, "minecraft:damage_type");
            write_varint(&mut packet, entries.len() as i64);
            for entry in entries {
                write_str(&mut packet, entry);
                packet.extend_from_slice(&[1, 10, 0]);
            }
            (0x07, packet)
        }
        let ids = damage_type_wire_ids_from_config(&[registry(&[
            "minecraft:drown",
            "minecraft:player_attack",
            "minecraft:mob_attack",
        ])]);
        assert_eq!(ids.get("minecraft:drown"), Some(&0));
        assert_eq!(ids.get("minecraft:player_attack"), Some(&1));
        assert_eq!(ids.get("minecraft:mob_attack"), Some(&2));
    }
}

// ---------- FIX-P0 trident / riptide / impaling / explosion / void ----------
//
// Every constant in this module was read out of the pinned 1.21.1 client jar:
// `TridentItem.releaseUsing` (the 10-tick draw gate, the wet gate, the
// `isTooDamagedToUse` guard, the look-vector impulse and
// `startAutoSpinAttack(20, 8.0F, stack)`), `ThrownTrident.tick`/`onHitEntity`
// (0.95 damping plus `0.05 * loyalty`, the typed damage terms, the
// `isAcceptibleReturnOwner` drop), `LivingEntity.checkAutoSpinAttack`,
// `ClientboundExplodePacket.write` and `Entity.checkBelowWorld`.
#[cfg(test)]
mod fix_p0_trident_tests {
    use super::*;
    use crate::effect_tests::client_pair;

    fn trident_arrow(eid: i32, owner: i32, dim: DimId, dmg: f32, loy: i64, comps: Comps) -> Arrow {
        let a = Arrow {
            eid,
            owner,
            x: 22.0,
            y: 70.9,
            z: 10.0,
            dx: 0.0,
            dy: 0.0,
            dz: 0.0,
            ttl: 100,
            dmg,
            fire: 0,
            punch: 0,
            kind: 1,
            loy,
            ret_item: 9999,
            ret_comps: comps,
            pierce: 0,
            last_hit: -1,
            stuck: false,
        };
        set_entity_dim(eid, dim);
        a
    }

    fn spin_mob(eid: i32, etype: i64) -> Mob {
        Mob {
            dim: DimId::Overworld,
            eid,
            uuid: [eid as u8; 16],
            etype,
            pos: Mutex::new((1.0, 64.0, 0.5, 0.0, 0.0)),
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

    /// `data/minecraft/enchantment/riptide.json`'s
    /// `trident_spin_attack_strength`: `add`/`linear` with base 1.5 and 0.75 per
    /// level above the first, i.e. `3.0 * (1 + level) / 4.0`.
    #[test]
    fn riptide_strength_matches_the_enchantment_data() {
        assert_eq!(riptide_strength(1), 1.5);
        assert_eq!(riptide_strength(2), 2.25);
        assert_eq!(riptide_strength(3), 3.0);
    }

    /// `TridentItem.releaseUsing`: a dry thrower is refused outright, a wet one
    /// is launched along its unit look vector at the enchantment strength and
    /// arms the 20-tick spin; a spent trident is refused even when wet.
    #[test]
    fn riptide_launch_needs_water_or_rain_and_an_usable_trident() {
        let look = [-1.0_f64, 0.0, 0.0];
        let wet = TridentSpecialCtx {
            riptide: 3,
            wet: true,
            look,
            ..Default::default()
        };
        let RiptideOutcome::Launch(launch) = evaluate_trident_special(&wet).riptide else {
            panic!("a wet thrower with Riptide III must launch");
        };
        assert!((launch.velocity[0] + 3.0).abs() < 1e-9, "{:?}", launch.velocity);
        assert_eq!(launch.velocity[1], 0.0);
        assert_eq!(launch.velocity[2], 0.0);
        assert_eq!(launch.spin_ticks, TRIDENT_SPIN_TICKS);
        assert_eq!(launch.spin_ticks, 20);
        assert_eq!(launch.ground_boost, 0.0);

        let dry = TridentSpecialCtx {
            riptide: 3,
            wet: false,
            look,
            ..Default::default()
        };
        assert_eq!(evaluate_trident_special(&dry).riptide, RiptideOutcome::Refused);

        let spent = TridentSpecialCtx {
            riptide: 3,
            wet: true,
            too_damaged: true,
            look,
            ..Default::default()
        };
        assert_eq!(
            evaluate_trident_special(&spent).riptide,
            RiptideOutcome::Refused,
            "isTooDamagedToUse refuses the release"
        );

        let plain = TridentSpecialCtx {
            riptide: 0,
            wet: true,
            look,
            ..Default::default()
        };
        assert_eq!(evaluate_trident_special(&plain).riptide, RiptideOutcome::NotRiptide);

        // The on-ground release adds vanilla's `move(SELF, 0..1.2..0)`.
        let grounded = TridentSpecialCtx {
            riptide: 1,
            wet: true,
            on_ground: true,
            look: [0.0, 0.0, 1.0],
            ..Default::default()
        };
        let RiptideOutcome::Launch(launch) = evaluate_trident_special(&grounded).riptide else {
            panic!("Riptide I must launch");
        };
        assert!((launch.velocity[2] - 1.5).abs() < 1e-9);
        assert!((launch.ground_boost - 1.2).abs() < 1e-9);
    }

    /// `TridentItem.isTooDamagedToUse` — `damage >= MAX_DAMAGE - 1`, so 249 of
    /// 250 is spent.
    #[test]
    fn trident_is_too_damaged_at_249_of_250() {
        assert_eq!(TRIDENT_MAX_DAMAGE, 250);
        let mut comps = Vec::new();
        for (damage, spent) in [(0_i64, false), (248, false), (249, true), (250, true)] {
            comp_set_varint(&mut comps, COMP_DAMAGE, damage);
            assert_eq!(trident_too_damaged(&comps), spent, "damage {damage}");
        }
    }

    /// The Impaling promotion: a thrown trident's own enchantments, gated on
    /// `#minecraft:sensitive_to_impaling`, decide the live damage.
    #[test]
    fn thrown_trident_impaling_is_tag_gated() {
        let cod = name_to_etype("cod").unwrap();
        let cow = name_to_etype("cow").unwrap();

        // Impaling III: `play_protocol` bakes `8.0 + 7.5` into the projectile.
        let mut enchanted = Vec::new();
        comp_set(
            &mut enchanted,
            COMP_ENCHANTMENTS,
            enchantments_data(&[(ENCHANT_IMPALING, 3)], true),
        );
        let a = trident_arrow(910_001, 42, DimId::Overworld, 15.5, 0, enchanted);
        assert_eq!(thrown_trident_base_damage(&a), 8.0);
        assert_eq!(thrown_trident_hit_damage(&a, Some(cod)), 15.5, "aquatic victim");
        assert_eq!(
            thrown_trident_hit_damage(&a, Some(cow)),
            8.0,
            "a cow is outside sensitive_to_impaling"
        );
        assert_eq!(thrown_trident_hit_damage(&a, None), 8.0, "players are never aquatic");

        // A plain trident keeps the flat base everywhere.
        let plain = trident_arrow(910_002, 42, DimId::Overworld, 8.0, 0, Vec::new());
        assert_eq!(thrown_trident_hit_damage(&plain, Some(cod)), 8.0);
        assert_eq!(thrown_trident_hit_damage(&plain, Some(cow)), 8.0);
        clear_entity_dim(910_001);
        clear_entity_dim(910_002);
    }

    /// `ThrownTrident.tick`'s return impulse: `dir.normalize() * 0.05 * loyalty`
    /// toward the owner's eye, never gated by the world floor.
    #[test]
    fn loyalty_impulse_is_five_percent_per_level_toward_the_owner() {
        let eval = evaluate_trident_special(&TridentSpecialCtx {
            loyalty: 3,
            trident: [0.0, f64::from(DimId::Overworld.min_y()) - 100.0, 0.0],
            owner: [0.0, 64.0, 0.0],
            ..Default::default()
        });
        let imp = eval.loyalty_return.expect("loyalty 3 must push home");
        // Straight up, 0.05 * 3.
        assert!((imp[1] - 0.15).abs() < 1e-9, "{imp:?}");
        assert_eq!(imp[0], 0.0);
        assert_eq!(imp[2], 0.0);
        // Loyalty 0 has no return impulse at all.
        let none = evaluate_trident_special(&TridentSpecialCtx {
            loyalty: 0,
            trident: [0.0, 40.0, 0.0],
            owner: [0.0, 64.0, 0.0],
            ..Default::default()
        });
        assert!(none.loyalty_return.is_none());
    }

    /// An owner who is gone never deletes the trident: `arrow_tick` lands it as
    /// an item entity in the trident's own dimension.
    #[test]
    fn loyalty_return_with_a_missing_owner_drops_the_trident_in_its_dim() {
        let _guard = crate::world_guard();
        TICK_IDX.store(4_242, Ordering::Relaxed);
        let eid = 910_010;
        let a = trident_arrow(eid, 910_011, DimId::Nether, 8.0, 2, Vec::new());
        lk(arrows()).push(a);
        lk(trident_returning()).insert(eid);
        lk(trident_seen()).insert(eid);
        lk(region()).hot.insert((1, 0));
        arrow_tick();
        lk(region()).hot.remove(&(1, 0));
        assert!(
            lk(arrows()).iter().all(|a| a.eid != eid),
            "a trident with no live owner leaves the flight list"
        );
        let landed = lk(drops())
            .iter()
            .find(|d| d.item == 9999)
            .map(|d| d.eid);
        assert!(
            landed.is_some(),
            "a trident with no live owner must land as an item, not vanish"
        );
        assert_eq!(
            drop_dim_of(landed.unwrap()),
            DimId::Nether,
            "the drop keeps the trident's dimension"
        );
        lk(drops()).retain(|d| d.item != 9999);
        clear_entity_dim(eid);
    }

    fn read_varint_at(bytes: &[u8], at: usize) -> (i64, usize) {
        let mut value = 0i64;
        let mut shift = 0;
        let mut i = at;
        loop {
            let byte = bytes[i];
            value |= i64::from(byte & 0x7f) << shift;
            i += 1;
            if byte & 0x80 == 0 {
                return (value, i);
            }
            shift += 7;
        }
    }

    fn explosion_packet_fields(
        block_interaction: i64,
    ) -> (f64, f64, f64, f32, Vec<(u8, u8, u8)>, [f32; 3], i64, i64, i64, i64, usize) {
        let records = [(1i8, -2i8, 3i8)];
        let bytes = explosion_pkt_with_power_and_interaction(
            1.5,
            64.25,
            -2.5,
            4.0,
            &records,
            block_interaction,
        );
        let x = f64::from_be_bytes(bytes[0..8].try_into().unwrap());
        let y = f64::from_be_bytes(bytes[8..16].try_into().unwrap());
        let z = f64::from_be_bytes(bytes[16..24].try_into().unwrap());
        let power = f32::from_be_bytes(bytes[24..28].try_into().unwrap());
        let (count, mut at) = read_varint_at(&bytes, 28);
        let mut cells = Vec::new();
        for _ in 0..count {
            cells.push((bytes[at], bytes[at + 1], bytes[at + 2]));
            at += 3;
        }
        let motion = [
            f32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()),
            f32::from_be_bytes(bytes[at + 4..at + 8].try_into().unwrap()),
            f32::from_be_bytes(bytes[at + 8..at + 12].try_into().unwrap()),
        ];
        at += 12;
        let (interaction, at) = read_varint_at(&bytes, at);
        let (small, at) = read_varint_at(&bytes, at);
        let (large, at) = read_varint_at(&bytes, at);
        let (sound, end) = read_varint_at(&bytes, at);
        (
            x,
            y,
            z,
            power,
            cells,
            motion,
            interaction,
            small,
            large,
            sound,
            end,
        )
    }

    /// `ClientboundExplodePacket.write`: `DDDF`, the cell list, three knockback
    /// floats, then the block-interaction ordinal, the small/large particle ids
    /// and the `Holder<SoundEvent>` value.
    #[test]
    fn explosion_packet_matches_the_1_21_1_wire() {
        for interaction in [
            EXPLOSION_BLOCK_INTERACTION_KEEP,
            EXPLOSION_BLOCK_INTERACTION_DESTROY_WITH_DECAY,
            EXPLOSION_BLOCK_INTERACTION_TRIGGER_BLOCK,
        ] {
            let (x, y, z, power, cells, motion, got, small, large, sound, end) =
                explosion_packet_fields(interaction);
            assert_eq!(x, 1.5);
            assert_eq!(y, 64.25);
            assert_eq!(z, -2.5);
            assert_eq!(power, 4.0);
            assert_eq!(cells, vec![(1, 0xfe, 3)]);
            assert_eq!(motion, [0.0; 3]);
            assert_eq!(got, interaction);
            assert_eq!(small, PARTICLE_EXPLOSION);
            assert_eq!(small, 22);
            assert_eq!(large, PARTICLE_EXPLOSION_EMITTER);
            assert_eq!(large, 21);
            assert_eq!(sound, SOUND_EVENT_GENERIC_EXPLODE_HOLDER);
            assert_eq!(sound, 577);
            let total = explosion_pkt_with_power_and_interaction(
                1.5,
                64.25,
                -2.5,
                4.0,
                &[(1, -2, 3)],
                interaction,
            )
            .len();
            assert_eq!(end, total, "no trailing bytes after the sound holder");
        }
        // KEEP (0) is vanilla's "nothing was removed"; the ordinal is not 1.
        assert_ne!(EXPLOSION_BLOCK_INTERACTION_KEEP, 1);
        // The wind-charge/wind-burst wrapper writes the TRIGGER interaction.
        let trigger = explosion_pkt_with_power(0.0, 0.0, 0.0, 1.2, &[]);
        let (_, at) = read_varint_at(&trigger, 28);
        let (interaction, _) = read_varint_at(&trigger, at);
        assert_eq!(interaction, EXPLOSION_BLOCK_INTERACTION_TRIGGER_BLOCK);
    }

    /// `Entity.checkBelowWorld`: the void arm starts at `min_y - 64`, so the
    /// Overworld floor is -128 while the Nether and End share -64.
    #[test]
    fn void_damage_floor_follows_the_dimension_min_y() {
        assert_eq!(void_damage_floor(DimId::Overworld), -128.0);
        assert_eq!(void_damage_floor(DimId::Nether), -64.0);
        assert_eq!(void_damage_floor(DimId::End), -64.0);
        assert_eq!(void_damage_floor(DimId::Overworld), f64::from(DimId::Overworld.min_y()) - 64.0);
    }

    #[test]
    fn void_damage_only_applies_below_the_dimension_floor() {
        let _guard = crate::world_guard();
        let (mut client, _sock) = client_pair();
        client.eid = 910_020;
        let client = Arc::new(client);
        let set_y = |y: f64| {
            let mut p = lk(&client.pos);
            p.1 = y;
        };
        *lk(&client.dim) = DimId::Overworld;
        set_y(-100.0);
        assert!(
            live_env_damage(&client, 4.0, "out_of_world").is_none(),
            "the Overworld void starts at -128, not -64"
        );
        set_y(-129.0);
        assert!(live_env_damage(&client, 4.0, "out_of_world").is_some());
        *lk(&client.dim) = DimId::Nether;
        set_y(-63.0);
        *lk(&client.invulnerable) = 0;
        assert!(
            live_env_damage(&client, 4.0, "out_of_world").is_none(),
            "the Nether floor is -64"
        );
        set_y(-65.0);
        *lk(&client.invulnerable) = 0;
        assert!(live_env_damage(&client, 4.0, "out_of_world").is_some());
        // A non-void source is untouched by the floor.
        *lk(&client.dim) = DimId::Overworld;
        set_y(64.0);
        *lk(&client.invulnerable) = 0;
        assert!(live_env_damage(&client, 1.0, "freeze").is_some());
    }

    /// Both per-mob side tables have a hard ceiling; the eviction helpers trim
    /// an over-full map back to it.
    #[test]
    fn mob_combat_side_tables_stay_bounded() {
        let mut hurt_amount: HashMap<i32, f32> = HashMap::new();
        for eid in 0..(MAX_MOB_LAST_HURT_ENTRIES as i32 + 8) {
            hurt_amount.insert(eid, 1.0);
        }
        bound_mob_last_hurt_amount(&mut hurt_amount);
        assert_eq!(hurt_amount.len(), MAX_MOB_LAST_HURT_ENTRIES);

        let mut effects: HashMap<i32, Vec<Eff>> = HashMap::new();
        for eid in 0..(MAX_MOB_EFFECTS_ENTRIES as i32 + 4) {
            effects.insert(
                eid,
                vec![Eff {
                    id: 10,
                    amp: 0,
                    left: 20,
                }],
            );
        }
        bound_mob_effects(&mut effects);
        assert_eq!(effects.len(), MAX_MOB_EFFECTS_ENTRIES);

        // A map at the ceiling is untouched.
        hurt_amount.remove(&0);
        bound_mob_last_hurt_amount(&mut hurt_amount);
        assert_eq!(hurt_amount.len(), MAX_MOB_LAST_HURT_ENTRIES);
        assert!(MAX_MOB_LAST_HURT_ENTRIES >= MOB_CAP);
    }

    /// `LivingEntity.checkAutoSpinAttack` + `Player.attack`: the first living
    /// mob inside the swept box takes `autoSpinAttackDmg` and ends the spin.
    #[test]
    fn spin_attack_pays_eight_and_ends_on_the_first_hit() {
        let _guard = crate::world_guard();
        TICK_IDX.store(9_000, Ordering::Relaxed);
        let (mut me, _sock) = client_pair();
        me.eid = 910_030;
        {
            let mut p = lk(&me.pos);
            p.0 = 0.5;
            p.1 = 64.0;
            p.2 = 0.5;
            p.3 = 0.0;
            p.4 = 0.0;
            p.5 = false;
        }
        let me = Arc::new(me);
        *lk(&me.lastp) = (0.5, 64.0, 0.5);
        lk(clients()).push(me.clone());
        let mob = Arc::new(spin_mob(910_031, name_to_etype("zombie").unwrap()));
        lk(mobs()).push(mob.clone());
        lk(riptide_spin()).insert(me.eid, TICK_IDX.load(Ordering::Relaxed) + server_ticks(20));
        spin_attack_step();
        let hp = *lk(&mob.hp);
        assert!(
            (hp - 12.0).abs() < 1e-3,
            "the spin pays autoSpinAttackDmg 8.0 (hp {hp})"
        );
        assert!(
            !lk(riptide_spin()).contains_key(&me.eid),
            "the spin ends on the first hit"
        );
        // Nothing left to hit: a second pass cannot re-damage.
        spin_attack_step();
        assert!((*lk(&mob.hp) - hp).abs() < 1e-6);
        lk(mobs()).clear();
        lk(clients()).clear();
        lk(mob_last_hurt_amount()).remove(&910_031);
        lk(mob_effects()).remove(&910_031);
        clear_entity_dim(910_031);
    }

    /// The channeling gate reads the *victim's* position: a dimension without
    /// sky light can never answer true.
    #[test]
    fn channeling_needs_sky_at_the_victim() {
        let a = Arrow {
            eid: 910_040,
            owner: 42,
            x: 0.0,
            y: 64.0,
            z: 0.0,
            dx: 0.0,
            dy: 0.0,
            dz: 0.0,
            ttl: 100,
            dmg: 8.0,
            fire: 0,
            punch: 1,
            kind: 1,
            loy: 0,
            ret_item: 0,
            ret_comps: Vec::new(),
            pierce: 0,
            last_hit: -1,
            stuck: false,
        };
        THUNDERING.store(200, Ordering::Relaxed);
        assert!(
            !arrow_channeling_hit(&a, 0.0, 64.0, 0.0, DimId::Nether),
            "the Nether has no sky light"
        );
        THUNDERING.store(0, Ordering::Relaxed);
        assert!(
            !arrow_channeling_hit(&a, 0.0, 64.0, 0.0, DimId::Overworld),
            "no thunder, no bolt"
        );
        // A non-trident projectile never channels.
        THUNDERING.store(200, Ordering::Relaxed);
        let mut plain = a;
        plain.kind = 0;
        assert!(!arrow_channeling_hit(&plain, 0.0, 64.0, 0.0, DimId::Overworld));
        THUNDERING.store(0, Ordering::Relaxed);
    }
}
