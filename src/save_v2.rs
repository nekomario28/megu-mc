/// A saved player position with its look direction *and* the dimension they logged out in. On disk
/// the dimension lives in the footer; the legacy six fields stay in their original wire layout.
type DimPosRot = (DimId, f64, f64, f64, f32, f32);

/// MET4 stores durations in server ticks. MET2 and MET3 stored the same counters as
/// 4TPS server ticks, which span five times the wall time, so a legacy value read
/// directly at 20TPS expires five times early. Dividing by TICK_MS converts the
/// legacy millisecond-equivalent budget, and TICK_MS went 250 to 50, so the factor
/// is exactly the cadence ratio.
fn legacy_ticks(raw: u64, legacy: bool) -> u64 {
    if legacy {
        raw / TICK_MS
    } else {
        raw
    }
}
/// The tick period MET2 and MET3 files were written at. It is a property of the
/// file format, not of the running server, so it is a fixed historical value and
/// must not be derived from TICK_MS.
const LEGACY_TICK_MS: u64 = 250;
/// Counters the legacy format stored as a number of 4TPS server ticks, not as
/// milliseconds: arrow and pearl ttl, primed TNT left, furnace burn_left and
/// progress, and the weather counters. A legacy tick spans 250ms, so it is five
/// 20TPS ticks; dividing would shorten the budget fivefold, which is the very
/// bug the legacy conversion exists to prevent.
fn legacy_tick_count(raw: u64, legacy: bool) -> u64 {
    if legacy {
        raw * (LEGACY_TICK_MS / TICK_MS)
    } else {
        raw
    }
}

static WORLD_SEED: AtomicU64 = AtomicU64::new(0);

static INV_STORE: OnceLock<Mutex<HashMap<String, HashMap<i16, Stack>>>> = OnceLock::new();
fn inv_store() -> &'static Mutex<HashMap<String, HashMap<i16, Stack>>> {
    INV_STORE.get_or_init(|| Mutex::new(HashMap::new()))
}
// name -> (x,y,z,yaw,pitch): vanilla respawns a player where they left
static POS_STORE: Registry<String, DimPosRot> = OnceLock::new();
/// The position record a client leaves behind: its live dimension plus the six legacy
/// fields. Both the logout write and the periodic world save call this, so a test can prove
/// the persisted dimension comes from `Client::dim` rather than a hard-coded Overworld.
fn client_dim_pos(c: &Client) -> DimPosRot {
    let dim = c.dim.lock().unwrap();
    let p = *c.pos.lock().unwrap();
    (*dim, p.0, p.1, p.2, p.3, p.4)
}
fn pos_store() -> &'static LockedMap<String, DimPosRot> {
    POS_STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

// Fixed-width signed values and IEEE floats are always big endian.
macro_rules! save_numbers {
    ($out:expr, $($value:expr),+ $(,)?) => {
        $($out.extend_from_slice(&$value.to_be_bytes());)+
    };
}

// The live respawn point currently exposes only a destination and angle. Keep
// that representation distinct from targets with explicit bed/anchor semantics.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SpawnTargetKind {
    Legacy,
    Bed,
    Anchor,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct SpawnTarget {
    kind: SpawnTargetKind,
    dim: DimId,
    position: (f64, f64, f64),
    angle: f32,
    forced: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct PlayerSave {
    position: DimPosRot,
    inventory: HashMap<i16, Stack>,
    ender_chest: HashMap<i16, Stack>,
    held: i16,
    health: f32,
    absorption: f32,
    food: i32,
    saturation: f32,
    exhaustion: f64,
    xp_level: i32,
    xp_progress: f32,
    xp_total: i64,
    gamemode: u8,
    effects: Vec<(i32, i32, i32)>,
    respawn: Option<SpawnTarget>,
}

// WORLD_SPAWN and the default spawn packet carry yaw. Keep pitch with the save.
static WORLD_SPAWN_PITCH: Mutex<f32> = Mutex::new(0.0);

fn saved_players() -> &'static LockedMap<[u8; 16], PlayerSave> {
    static PLAYERS: Registry<[u8; 16], PlayerSave> = OnceLock::new();
    PLAYERS.get_or_init(Mutex::default)
}

fn saved_value<T: Copy>(value: &Mutex<T>) -> T {
    *value.lock().unwrap()
}

impl PlayerSave {
    fn capture(client: &Client, position: DimPosRot, inventory: &HashMap<i16, Stack>) -> Self {
        let (xp_total, (xp_level, xp_progress)) = {
            let total = client.xp.lock().unwrap_or_else(|e| e.into_inner());
            let sessions = player_mechanism_sessions().lock().unwrap_or_else(|e| e.into_inner());
            (
                *total,
                sessions.get(&client.eid).map_or((0, 0.0), |s| s.xp_bar),
            )
        };
        let respawn = {
            let dim = client.spawn_dim.lock().unwrap_or_else(|e| e.into_inner());
            saved_value(&client.spawn_pt).map(|(x, y, z, angle)| {
                let live = SpawnTarget {
                    kind: SpawnTargetKind::Legacy,
                    dim: *dim,
                    position: (x, y, z),
                    angle,
                    forced: false,
                };
                // The live client only tracks the destination. When a bed or
                // respawn anchor record was already saved for this exact point,
                // the autosave's recapture keeps that record instead of
                // flattening the kind back to Legacy.
                saved_players()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&client.uuid)
                    .and_then(|save| save.respawn)
                    .filter(|saved| {
                        saved.dim == live.dim
                            && saved.position == live.position
                            && saved.angle == live.angle
                    })
                    .unwrap_or(live)
            })
        };
        let ender_chest = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ender_container_key(&client.uuid))
            .map(|slots| saved_slots(slots, 0..27))
            .unwrap_or_default();
        let effects = client
            .effs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|effect| (effect.id, effect.amp, effect.left))
            .collect();
        Self {
            position,
            // Window 0 slots 5–8 are armor, 9–44 inventory, and 45 offhand.
            inventory: saved_slots(inventory, 5..46),
            ender_chest,
            held: saved_value(&client.held),
            health: saved_value(&client.hp),
            absorption: saved_value(&client.absorb),
            food: saved_value(&client.food),
            saturation: saved_value(&client.sat),
            exhaustion: saved_value(&client.walk_acc),
            xp_level,
            xp_progress,
            xp_total,
            gamemode: saved_value(&client.gm),
            effects,
            respawn,
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        let (dim, x, y, z, yaw, pitch) = self.position;
        out.push(legacy_dim_byte(dim));
        save_numbers!(out, x, y, z, yaw, pitch);
        write_saved_slots(out, &self.inventory);
        write_saved_slots(out, &self.ender_chest);
        save_numbers!(
            out,
            self.held,
            self.health,
            self.absorption,
            self.food,
            self.saturation,
            self.exhaustion,
            self.xp_level,
            self.xp_progress,
            self.xp_total
        );
        out.push(self.gamemode);
        write_varint(out, self.effects.len() as i64);
        for &(id, amp, left) in &self.effects {
            save_numbers!(out, id, amp, left);
        }
        if let Some(point) = self.respawn {
            out.push(match point.kind {
                SpawnTargetKind::Legacy => 1,
                SpawnTargetKind::Bed => 2,
                SpawnTargetKind::Anchor => 3,
            });
            out.push(legacy_dim_byte(point.dim));
            let (x, y, z) = point.position;
            save_numbers!(out, x, y, z, point.angle);
            out.push(u8::from(point.forced));
        } else {
            out.push(0);
        }
    }

    fn read(input: &mut SaveInput<'_>) -> Result<Self, &'static str> {
        let dim = input.dim()?;
        let (x, y, z) = input.position()?;
        let yaw = input.f32()?;
        let pitch = input.f32()?;
        let inventory = input.slots(5..46)?;
        let ender_chest = input.slots(0..27)?;
        let held = i16::from_be_bytes(input.take()?);
        let health = input.f32()?;
        let absorption = input.f32()?;
        let food = i32::from_be_bytes(input.take()?);
        let saturation = input.f32()?;
        let exhaustion = input.f64()?;
        let xp_level = i32::from_be_bytes(input.take()?);
        let xp_progress = input.f32()?;
        let xp_total = i64::from_be_bytes(input.take()?);
        let gamemode = input.byte()?;
        if !(0..=8).contains(&held) || gamemode > 3 {
            return Err("invalid saved player slot or game mode");
        }
        let count = input.var_count(12)?;
        let mut effects = Vec::new();
        for _ in 0..count {
            effects.push((
                i32::from_be_bytes(input.take()?),
                i32::from_be_bytes(input.take()?),
                i32::from_be_bytes(input.take()?),
            ));
        }
        let respawn = match input.byte()? {
            0 => None,
            tag @ 1..=3 => Some(SpawnTarget {
                kind: [
                    SpawnTargetKind::Legacy,
                    SpawnTargetKind::Bed,
                    SpawnTargetKind::Anchor,
                ][usize::from(tag - 1)],
                dim: input.dim()?,
                position: input.position()?,
                angle: input.f32()?,
                forced: input.flag()?,
            }),
            _ => return Err("invalid saved respawn target"),
        };
        Ok(Self {
            position: (dim, x, y, z, yaw, pitch),
            inventory,
            ender_chest,
            held,
            health,
            absorption,
            food,
            saturation,
            exhaustion,
            xp_level,
            xp_progress,
            xp_total,
            gamemode,
            effects,
            respawn,
        })
    }
}

fn saved_slots(slots: &HashMap<i16, Stack>, range: std::ops::Range<i16>) -> HashMap<i16, Stack> {
    range
        .filter_map(|slot| {
            slots
                .get(&slot)
                .filter(|stack| stack.1 != 0)
                .map(|s| (slot, s.clone()))
        })
        .collect()
}

fn write_saved_slots(out: &mut Vec<u8>, slots: &HashMap<i16, Stack>) {
    let mut entries: Vec<_> = slots.iter().collect();
    entries.sort_unstable_by_key(|(&slot, _)| slot);
    write_varint(out, entries.len() as i64);
    for (&slot, stack) in entries {
        save_numbers!(out, slot);
        write_saved_stack(out, stack);
    }
}

fn write_saved_stack(out: &mut Vec<u8>, (item, count, comps): &Stack) {
    write_varint(out, *item);
    out.push(*count);
    write_saved_components(out, comps);
}

fn write_saved_components(out: &mut Vec<u8>, comps: &Comps) {
    write_varint(out, comps.len() as i64);
    for (ty, data) in comps {
        write_varint(out, *ty);
        write_varint(out, data.len() as i64);
        out.extend_from_slice(data);
    }
}

// Mob records follow the MET4 ordinals. IDs visible to clients are re-issued
// on load, so none of the serialized state may refer to an old runtime ID.
struct MobSave {
    uuid: [u8; 16],
    etype: i64,
    yaw: f32,
    pitch: f32,
    vel: [f64; 3],
    fuse: i32,
    fire_ticks: i32,
    love: i32,
    air: i32,
    state: LiveMobState,
    villager: Option<Villager>,
    sitting_cat: bool,
}

const SAVED_WITCH_POTIONS: [WitchPotion; 8] = [
    WitchPotion::WaterBreathing,
    WitchPotion::FireResistance,
    WitchPotion::Healing,
    WitchPotion::Swiftness,
    WitchPotion::Slowness,
    WitchPotion::Poison,
    WitchPotion::Weakness,
    WitchPotion::Harming,
];

fn write_saved_potion(out: &mut Vec<u8>, potion: WitchPotion) {
    let index = SAVED_WITCH_POTIONS
        .iter()
        .position(|&p| p == potion)
        .unwrap();
    out.push(index as u8);
}

fn read_saved_potion(input: &mut SaveInput<'_>) -> Result<WitchPotion, &'static str> {
    SAVED_WITCH_POTIONS
        .get(usize::from(input.byte()?))
        .copied()
        .ok_or("invalid saved mob potion")
}

impl MobSave {
    fn write(out: &mut Vec<u8>, mob: &Mob, yaw: f32, pitch: f32, now: u64) {
        out.extend_from_slice(&mob.uuid);
        write_varint(out, mob.etype);
        let [dx, dy, dz] = saved_value(&mob.vel);
        save_numbers!(
            out,
            yaw,
            pitch,
            dx,
            dy,
            dz,
            saved_value(&mob.fuse),
            saved_value(&mob.fire_ticks),
            saved_value(&mob.love),
            saved_value(&mob.air)
        );
        out.push(u8::from(sitting_cats().lock().unwrap_or_else(|e| e.into_inner()).contains(&mob.eid)));
        Self::write_behavior(out, live_mob_states().lock().unwrap_or_else(|e| e.into_inner()).get(&mob.eid), now);
        let villagers = live_villagers().lock().unwrap_or_else(|e| e.into_inner());
        let villager = villagers.get(&mob.eid);
        out.push(u8::from(villager.is_some()));
        if let Some(villager) = villager {
            write_saved_villager(out, villager);
        }
    }

    fn write_behavior(out: &mut Vec<u8>, state: Option<&LiveMobState>, now: u64) {
        if let Some(state) = state {
            out.push(1);
            out.push(u8::from(state.blaze_charged));
            let effects: Vec<_> = state
                .potions
                .iter()
                .filter(|(_, until)| *until > now)
                .collect();
            write_varint(out, effects.len() as i64);
            for &(potion, until) in effects {
                write_saved_potion(out, potion);
                save_numbers!(out, until - now);
            }
            match &state.behavior {
                Some(MobBehaviorState::Wolf(pet) | MobBehaviorState::Cat(pet)) => {
                    write_optional_uuid(out, pet.owner);
                    save_numbers!(out, pet.hp);
                    out.extend([u8::from(pet.sitting), pet.collar_color]);
                }
                Some(MobBehaviorState::Slime {
                    behavior,
                    split_roll,
                }) => {
                    out.extend([behavior.size as u8, *split_roll]);
                }
                Some(MobBehaviorState::Piglin(piglin)) => {
                    save_numbers!(out, piglin.conversion.unsafe_ticks);
                }
                Some(MobBehaviorState::Hoglin(hoglin)) => {
                    save_numbers!(out, hoglin.conversion.unsafe_ticks);
                }
                Some(MobBehaviorState::Blaze(blaze)) => {
                    let (phase, ticks, shots) = match blaze.phase {
                        BlazePhase::Idle(ticks) => (0, ticks, 0),
                        BlazePhase::Charging(ticks) => (1, ticks, 0),
                        BlazePhase::Burst { ticks, shots } => (2, ticks, shots),
                        BlazePhase::Cooldown(ticks) => (3, ticks, 0),
                    };
                    out.extend([phase, ticks, shots, blaze.melee_cooldown]);
                }
                Some(MobBehaviorState::Ghast(ghast)) => {
                    save_numbers!(out, ghast.altitude, ghast.attack_tick);
                }
                Some(MobBehaviorState::Shulker(shulker)) => {
                    out.extend([shulker.open_ticks, shulker.shot_ticks]);
                }
                Some(MobBehaviorState::Witch(witch)) => {
                    out.push(u8::from(witch.drinking.is_some()));
                    if let Some((ticks, potion)) = witch.drinking {
                        out.push(ticks);
                        write_saved_potion(out, potion);
                    }
                }
                Some(MobBehaviorState::Wither(wither)) => {
                    save_numbers!(out, wither.hp, wither.spawn_ticks);
                }
                Some(MobBehaviorState::Warden(warden)) => {
                    save_numbers!(out, warden.hp);
                    out.push(warden.sonic_cooldown);
                }
                _ => {}
            }
        } else {
            out.push(0);
        }
    }

    fn read(input: &mut SaveInput<'_>) -> Result<Self, &'static str> {
        let uuid = input.take()?;
        let etype = input.varint()?;
        let yaw = input.f32()?;
        let pitch = input.f32()?;
        let vel = [input.f64()?, input.f64()?, input.f64()?];
        let fuse = i32::from_be_bytes(input.take()?);
        let fire_ticks = i32::from_be_bytes(input.take()?);
        let love = i32::from_be_bytes(input.take()?);
        let air = i32::from_be_bytes(input.take()?);
        let sitting_cat = input.flag()?;
        let mut state = LiveMobState::new(etype);
        if input.flag()? {
            state.blaze_charged = input.flag()?;
            let effects = input.var_count(9)?;
            for _ in 0..effects {
                let potion = read_saved_potion(input)?;
                let remaining = u64::from_be_bytes(input.take()?);
                if !(1..=i32::MAX as u64).contains(&remaining) {
                    return Err("invalid saved mob effect duration");
                }
                state.potions.push((potion, remaining));
            }
            match &mut state.behavior {
                Some(MobBehaviorState::Wolf(pet) | MobBehaviorState::Cat(pet)) => {
                    pet.owner = input.optional_uuid()?;
                    pet.hp = input.f32()?;
                    pet.sitting = input.flag()?;
                    pet.collar_color = input.byte()?;
                    if pet.collar_color > 15 {
                        return Err("invalid saved pet collar");
                    }
                }
                Some(MobBehaviorState::Slime {
                    behavior,
                    split_roll,
                }) => {
                    behavior.size = match input.byte()? {
                        1 => SlimeSize::Small,
                        2 => SlimeSize::Medium,
                        4 => SlimeSize::Large,
                        _ => return Err("invalid saved slime size"),
                    };
                    *split_roll = input.byte()?;
                }
                Some(MobBehaviorState::Piglin(piglin)) => {
                    piglin.conversion.unsafe_ticks = u16::from_be_bytes(input.take()?);
                }
                Some(MobBehaviorState::Hoglin(hoglin)) => {
                    hoglin.conversion.unsafe_ticks = u16::from_be_bytes(input.take()?);
                }
                Some(MobBehaviorState::Blaze(blaze)) => {
                    let [phase, ticks, shots, cooldown] = input.take()?;
                    blaze.phase = match phase {
                        0 if ticks > 0 => BlazePhase::Idle(ticks),
                        1 if ticks > 0 => BlazePhase::Charging(ticks),
                        2 if ticks > 0 && (1..=3).contains(&shots) => {
                            BlazePhase::Burst { ticks, shots }
                        }
                        3 if ticks > 0 => BlazePhase::Cooldown(ticks),
                        _ => return Err("invalid saved blaze phase"),
                    };
                    blaze.melee_cooldown = cooldown;
                }
                Some(MobBehaviorState::Ghast(ghast)) => {
                    ghast.altitude = input.f64()?;
                    ghast.attack_tick = i8::from_be_bytes(input.take()?);
                    if !(-40..20).contains(&ghast.attack_tick) {
                        return Err("invalid saved ghast attack tick");
                    }
                }
                Some(MobBehaviorState::Shulker(shulker)) => {
                    let [open, shot] = input.take()?;
                    shulker.open_ticks = open;
                    shulker.shot_ticks = shot;
                }
                Some(MobBehaviorState::Witch(witch)) => {
                    if input.flag()? {
                        let ticks = input.byte()?;
                        if !(1..=32).contains(&ticks) {
                            return Err("invalid saved witch drinking time");
                        }
                        witch.drinking = Some((ticks, read_saved_potion(input)?));
                    }
                }
                Some(MobBehaviorState::Wither(wither)) => {
                    wither.hp = input.f32()?;
                    wither.spawn_ticks = u16::from_be_bytes(input.take()?);
                }
                Some(MobBehaviorState::Warden(warden)) => {
                    warden.hp = input.f32()?;
                    warden.sonic_cooldown = input.byte()?;
                }
                _ => {}
            }
        }
        let villager = if input.flag()? {
            Some(read_saved_villager(input)?)
        } else {
            None
        };
        Ok(Self {
            uuid,
            etype,
            yaw,
            pitch,
            vel,
            fuse,
            fire_ticks,
            love,
            air,
            state,
            villager,
            sitting_cat,
        })
    }
}

fn write_optional_uuid(out: &mut Vec<u8>, uuid: Option<[u8; 16]>) {
    out.push(u8::from(uuid.is_some()));
    if let Some(uuid) = uuid {
        out.extend_from_slice(&uuid);
    }
}

const SAVED_PROFESSIONS: [VillagerProfession; 15] = {
    use VillagerProfession::*;
    [
        None,
        Armorer,
        Butcher,
        Cartographer,
        Cleric,
        Farmer,
        Fisherman,
        Fletcher,
        Leatherworker,
        Librarian,
        Mason,
        Nitwit,
        Shepherd,
        Toolsmith,
        Weaponsmith,
    ]
};

fn write_saved_block_pos(out: &mut Vec<u8>, (x, y, z): BlockPos) {
    save_numbers!(out, x, y, z);
}

fn write_saved_villager(out: &mut Vec<u8>, villager: &Villager) {
    out.extend_from_slice(&villager.id);
    let profession = SAVED_PROFESSIONS
        .iter()
        .position(|&p| p == villager.profession)
        .unwrap();
    out.push(profession as u8);
    // The level is derived from XP in the live merchant.
    save_numbers!(out, villager.villager_xp);
    for pos in [villager.home, villager.job_site, villager.meeting_point] {
        out.push(u8::from(pos.is_some()));
        if let Some(pos) = pos {
            write_saved_block_pos(out, pos);
        }
    }
    out.extend([u8::from(villager.sleeping), villager.restocks_today]);
    for tick in [villager.day, villager.last_restock, villager.last_gossip] {
        out.push(u8::from(tick.is_some()));
        if let Some(tick) = tick {
            save_numbers!(out, tick);
        }
    }
    let write_cost = |out: &mut Vec<u8>, cost: &ItemCost| {
        write_varint(out, cost.item);
        save_numbers!(out, cost.count);
        write_saved_components(out, &cost.components);
    };
    write_varint(out, villager.offers.len() as i64);
    for offer in &villager.offers {
        write_cost(out, &offer.cost_a);
        out.push(u8::from(offer.cost_b.is_some()));
        if let Some(cost) = &offer.cost_b {
            write_cost(out, cost);
        }
        write_saved_stack(out, &offer.result);
        save_numbers!(
            out,
            offer.uses,
            offer.max_uses,
            offer.xp_reward,
            offer.price_multiplier,
            offer.demand,
            offer.special_price_diff
        );
    }
    let mut gossip: Vec<_> = villager.gossip.by_player.iter().collect();
    gossip.sort_unstable_by_key(|&(&uuid, _)| uuid);
    write_varint(out, gossip.len() as i64);
    for (uuid, values) in gossip {
        out.extend_from_slice(uuid);
        for value in values {
            save_numbers!(out, value);
        }
    }
}

fn read_saved_villager(input: &mut SaveInput<'_>) -> Result<Villager, &'static str> {
    let id = input.take()?;
    let profession = *SAVED_PROFESSIONS
        .get(usize::from(input.byte()?))
        .ok_or("invalid saved villager profession")?;
    let villager_xp = i32::from_be_bytes(input.take()?);
    let home = input.optional_block_pos()?;
    let job_site = input.optional_block_pos()?;
    let meeting_point = input.optional_block_pos()?;
    let sleeping = input.flag()?;
    let restocks_today = input.byte()?;
    let day = input.optional_tick()?;
    let last_restock = input.optional_tick()?;
    let last_gossip = input.optional_tick()?;
    let read_cost = |input: &mut SaveInput<'_>| {
        let item = input.varint()?;
        let count = i32::from_be_bytes(input.take()?);
        if item <= 0 || count <= 0 {
            return Err("invalid saved villager trade cost");
        }
        Ok(ItemCost {
            item,
            count,
            components: input.components()?,
        })
    };
    let count = input.var_count(34)?;
    let mut offers = Vec::new();
    for _ in 0..count {
        let cost_a = read_cost(input)?;
        let cost_b = if input.flag()? {
            Some(read_cost(input)?)
        } else {
            None
        };
        offers.push(MerchantOffer {
            cost_a,
            cost_b,
            result: input.stack()?,
            uses: i32::from_be_bytes(input.take()?),
            max_uses: i32::from_be_bytes(input.take()?),
            xp_reward: i32::from_be_bytes(input.take()?),
            price_multiplier: input.f32()?,
            demand: i32::from_be_bytes(input.take()?),
            special_price_diff: i32::from_be_bytes(input.take()?),
        });
    }
    let count = input.var_count(16 + 4 * GossipType::ALL.len())?;
    let mut gossip = VillagerGossip::default();
    for _ in 0..count {
        let uuid = input.take()?;
        let mut values = [0; GossipType::ALL.len()];
        for (value, kind) in values.iter_mut().zip(GossipType::ALL) {
            *value = i32::from_be_bytes(input.take()?);
            if !(0..=kind.rules().1).contains(value) {
                return Err("invalid saved gossip value");
            }
        }
        if gossip.by_player.insert(uuid, values).is_some() {
            return Err("duplicate saved gossip player");
        }
    }
    Ok(Villager {
        id,
        profession,
        villager_xp,
        home,
        job_site,
        meeting_point,
        sleeping,
        offers,
        trading_player: None,
        gossip,
        day,
        restocks_today,
        last_restock,
        last_gossip,
    })
}

fn saved_raids() -> &'static Mutex<Vec<(DimId, Raid)>> {
    static RAIDS: OnceLock<Mutex<Vec<(DimId, Raid)>>> = OnceLock::new();
    RAIDS.get_or_init(Mutex::default)
}

const SAVED_RAID_STATES: [RaidState; 5] = [
    RaidState::Celebrating,
    RaidState::InProgress,
    RaidState::Victory,
    RaidState::Loss,
    RaidState::Stopped,
];

fn write_saved_raid(out: &mut Vec<u8>, dim: DimId, raid: &Raid) {
    out.push(legacy_dim_byte(dim));
    write_saved_block_pos(out, raid.center);
    write_saved_block_pos(out, raid.origin);
    let state = SAVED_RAID_STATES
        .iter()
        .position(|&s| s == raid.state)
        .unwrap();
    out.extend([
        state as u8,
        raid.regular_waves,
        raid.total_waves,
        raid.waves_spawned,
        raid.omen_level,
    ]);
    save_numbers!(out, raid.countdown, raid.ticks_elapsed);
    let mut remaining: Vec<_> = raid.remaining.iter().collect();
    remaining.sort_unstable_by_key(|r| (r.wave, r.slot));
    write_varint(out, remaining.len() as i64);
    for raider in remaining {
        out.push(raider.wave);
        write_varint(out, raider.slot as i64);
    }
    write_varint(out, raid.participants.len() as i64);
    for uuid in &raid.participants {
        out.extend_from_slice(uuid);
    }
}

fn read_saved_raid(input: &mut SaveInput<'_>) -> Result<(DimId, Raid), &'static str> {
    let dim = input.dim()?;
    let center = input.block_pos()?;
    let origin = input.block_pos()?;
    let [state, regular_waves, total_waves, waves_spawned, omen_level] = input.take()?;
    let state = *SAVED_RAID_STATES
        .get(usize::from(state))
        .ok_or("invalid saved raid state")?;
    let countdown = u16::from_be_bytes(input.take()?);
    let ticks_elapsed = u32::from_be_bytes(input.take()?);
    if !matches!(regular_waves, 3 | 5 | 7)
        || !(1..=5).contains(&omen_level)
        || total_waves != regular_waves + u8::from(omen_level > 1)
        || waves_spawned > total_waves
        || (countdown == 0 && state != RaidState::Stopped)
        || countdown > RAID_CELEBRATION_TICKS
    {
        return Err("invalid saved raid progress");
    }
    let count = input.var_count(2)?;
    let mut remaining = HashSet::new();
    for _ in 0..count {
        let wave = input.byte()?;
        let slot = usize::try_from(input.varint()?).map_err(|_| "invalid saved raider slot")?;
        if wave == 0 || wave != waves_spawned || state != RaidState::InProgress {
            return Err("invalid saved raider wave");
        }
        let row = RAID_BASE_WAVES[usize::from(wave.min(regular_waves) - 1)];
        let slots = row.into_iter().map(usize::from).sum();
        if slot >= slots || !remaining.insert(RaidRaiderId { wave, slot }) {
            return Err("invalid or duplicate saved raider");
        }
    }
    let count = input.var_count(16)?;
    let mut participants = std::collections::BTreeSet::new();
    for _ in 0..count {
        if !participants.insert(input.take()?) {
            return Err("duplicate saved raid participant");
        }
    }
    Ok((
        dim,
        Raid {
            center,
            origin,
            state,
            regular_waves,
            total_waves,
            waves_spawned,
            countdown,
            ticks_elapsed,
            remaining,
            participants,
            omen_level,
        },
    ))
}

// The new extension sits between the legacy BRD1/SPA1 region and GEN1.
// Outer readers still peel the same SED1, MDM1, BRS1 and dimension footers.
const SAVE_STATE_MAGIC: &[u8; 4] = b"SVP1";
const SAVE_ENTITY_RULES_MAGIC: &[u8; 4] = b"SVX1";

// The live rules have dedicated atomics. All other rules live in the
// command registry; capture the atomics as authoritative when saving.
fn gamerule_value(name: &str, rules: &HashMap<&'static str, i32>) -> i32 {
    gamerule_atomic_value(name).unwrap_or(rules[name])
}

/// The eight rules a pre-SVP1 save could carry: their runtime values lived in
/// the atomics the file format never wrote, so a legacy load keeps the
/// atomics authoritative for them. Every other rule has no legacy carrier —
/// it restores to its default.
const LEGACY_ATOMIC_GAMERULES: &[&str] = &[
    "keepInventory",
    "doDaylightCycle",
    "doWeatherCycle",
    "doMobSpawning",
    "mobGriefing",
    "naturalRegeneration",
    "doMobLoot",
    "maxEntityCramming",
];

fn restore_gamerules(saved: Option<&[i32]>) {
    let mut rules = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
    rules.clear();
    rules.extend(COMMAND_GAMERULE_DEFAULTS.iter().map(|&(name, _, default)| (name, default)));
    for (index, &(name, _, _)) in COMMAND_GAMERULE_DEFAULTS.iter().enumerate() {
        let value = saved.map_or_else(
            || {
                if LEGACY_ATOMIC_GAMERULES.contains(&name) {
                    gamerule_value(name, &rules)
                } else {
                    rules[name]
                }
            },
            |values| values[index],
        );
        rules.insert(name, value);
        gamerule_atomic_store(name, value);
    }
}

fn split_save_footer<'a>(
    payload: &'a [u8],
    magic: &[u8; 4],
) -> Result<(&'a [u8], Option<&'a [u8]>), &'static str> {
    if !payload.ends_with(magic) {
        return Ok((payload, None));
    }
    let end = payload
        .len()
        .checked_sub(12)
        .ok_or("truncated save footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid save footer size")?;
    let start = end.checked_sub(size).ok_or("invalid save footer size")?;
    Ok((&payload[..start], Some(&payload[start..end])))
}

// Optional [granted players][core criterion progress][datapack criterion progress]
// followed by [u64 body length][ADV1]. Names use u16 byte lengths and UTF-8;
// core criteria carry signed big-endian i64 earned-at milliseconds.
const ADVANCEMENTS_MAGIC: &[u8; 4] = b"ADV1";
const ADV_MAX_PLAYERS: usize = 16_384;
const ADV_MAX_IDS: usize = 4_096;
const ADV_MAX_CRITERIA: usize = 256;
const ADV_MAX_NAME_BYTES: usize = 4_096;

struct SavedAdvancements {
    granted: HashMap<String, HashSet<String>>,
    core: AdvancementTracker,
    datapack: Advancements,
}

fn write_advancement_string(out: &mut Vec<u8>, value: &str) {
    assert!(!value.is_empty() && value.len() <= ADV_MAX_NAME_BYTES, "invalid advancement string length");
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn read_advancement_string(input: &mut SaveInput<'_>) -> Result<String, &'static str> {
    let len = u16::from_be_bytes(input.take()?) as usize;
    if len == 0 || len > ADV_MAX_NAME_BYTES {
        return Err("invalid advancement string length");
    }
    std::str::from_utf8(input.bytes(len)?)
        .map(str::to_owned)
        .map_err(|_| "invalid advancement UTF-8")
}

fn advancement_count(input: &mut SaveInput<'_>, max: usize) -> Result<usize, &'static str> {
    let count = input.var_count(1)?;
    if count > max {
        return Err("invalid advancement count");
    }
    Ok(count)
}

fn append_advancements_footer(out: &mut Vec<u8>) {
    let granted = adv_done().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let core = player_advancements().lock().unwrap_or_else(|e| e.into_inner()).progress.clone();
    let datapack = adv_prog().lock().unwrap_or_else(|e| e.into_inner()).clone();
    if granted.values().all(HashSet::is_empty)
        && core.values().all(HashMap::is_empty)
        && datapack.values().all(HashMap::is_empty)
    {
        return;
    }
    let start = out.len();
    assert!(granted.len() <= ADV_MAX_PLAYERS && core.len() <= ADV_MAX_PLAYERS
        && datapack.len() <= ADV_MAX_PLAYERS, "too many advancement players");
    let mut players: Vec<_> = granted.iter().filter(|(_, ids)| !ids.is_empty()).collect();
    players.sort_unstable_by(|a, b| a.0.cmp(b.0));
    write_varint(out, players.len() as i64);
    for (player, ids) in players {
        write_advancement_string(out, player);
        assert!(ids.len() <= ADV_MAX_IDS, "too many granted advancements");
        let mut ids: Vec<_> = ids.iter().collect();
        ids.sort_unstable();
        write_varint(out, ids.len() as i64);
        for id in ids {
            write_advancement_string(out, id);
        }
    }

    let mut players: Vec<_> = core.iter().filter(|(_, rows)| !rows.is_empty()).collect();
    players.sort_unstable_by(|a, b| a.0.cmp(b.0));
    write_varint(out, players.len() as i64);
    for (player, rows) in players {
        write_advancement_string(out, player);
        assert!(rows.len() <= ADV_MAX_IDS, "too many core advancements");
        let mut ids: Vec<_> = rows.iter().collect();
        ids.sort_unstable_by_key(|&(id, _)| id);
        write_varint(out, ids.len() as i64);
        for (id, criteria) in ids {
            write_advancement_string(out, id);
            assert!(criteria.len() <= ADV_MAX_CRITERIA, "too many advancement criteria");
            write_varint(out, criteria.len() as i64);
            for (criterion, timestamp) in criteria {
                write_advancement_string(out, criterion);
                out.extend_from_slice(&timestamp.to_be_bytes());
            }
        }
    }

    let mut players: Vec<_> = datapack.iter().filter(|(_, rows)| !rows.is_empty()).collect();
    players.sort_unstable_by(|a, b| a.0.cmp(b.0));
    write_varint(out, players.len() as i64);
    for (player, rows) in players {
        write_advancement_string(out, player);
        assert!(rows.len() <= ADV_MAX_IDS, "too many datapack advancements");
        let mut ids: Vec<_> = rows.iter().collect();
        ids.sort_unstable_by(|a, b| a.0.cmp(b.0));
        write_varint(out, ids.len() as i64);
        for (id, criteria) in ids {
            write_advancement_string(out, id);
            assert!(criteria.len() <= ADV_MAX_CRITERIA, "too many datapack criteria");
            let mut criteria: Vec<_> = criteria.iter().collect();
            criteria.sort_unstable();
            write_varint(out, criteria.len() as i64);
            for criterion in criteria {
                write_advancement_string(out, criterion);
            }
        }
    }
    out.extend_from_slice(&((out.len() - start) as u64).to_be_bytes());
    out.extend_from_slice(ADVANCEMENTS_MAGIC);
}

fn read_advancements_body(body: &[u8]) -> Result<SavedAdvancements, &'static str> {
    let mut input = SaveInput(body);
    let mut granted = HashMap::new();
    for _ in 0..advancement_count(&mut input, ADV_MAX_PLAYERS)? {
        let player = read_advancement_string(&mut input)?;
        let mut ids = HashSet::new();
        for _ in 0..advancement_count(&mut input, ADV_MAX_IDS)? {
            if !ids.insert(read_advancement_string(&mut input)?) {
                return Err("duplicate granted advancement");
            }
        }
        if ids.is_empty() || granted.insert(player, ids).is_some() {
            return Err("duplicate or empty advancement player");
        }
    }

    let mut core = AdvancementTracker::default();
    for _ in 0..advancement_count(&mut input, ADV_MAX_PLAYERS)? {
        let player = read_advancement_string(&mut input)?;
        if core.progress.contains_key(&player) {
            return Err("duplicate core advancement player");
        }
        let count = advancement_count(&mut input, ADV_MAX_IDS)?;
        if count == 0 {
            return Err("empty core advancement player");
        }
        for _ in 0..count {
            let id = read_advancement_string(&mut input)?;
            if core.progress.get(&player).is_some_and(|rows| rows.contains_key(id.as_str())) {
                return Err("duplicate core advancement");
            }
            let criteria = advancement_count(&mut input, ADV_MAX_CRITERIA)?;
            if criteria == 0 {
                return Err("empty core advancement");
            }
            for _ in 0..criteria {
                let criterion = read_advancement_string(&mut input)?;
                let timestamp = i64::from_be_bytes(input.take()?);
                if !core.grant_criterion(&player, &id, &criterion, timestamp) {
                    return Err("invalid or duplicate core advancement criterion");
                }
            }
        }
    }

    let mut datapack = HashMap::new();
    for _ in 0..advancement_count(&mut input, ADV_MAX_PLAYERS)? {
        let player = read_advancement_string(&mut input)?;
        let mut rows = HashMap::new();
        let count = advancement_count(&mut input, ADV_MAX_IDS)?;
        if count == 0 {
            return Err("empty datapack advancement player");
        }
        for _ in 0..count {
            let id = read_advancement_string(&mut input)?;
            let mut criteria = HashSet::new();
            let count = advancement_count(&mut input, ADV_MAX_CRITERIA)?;
            if count == 0 {
                return Err("empty datapack advancement");
            }
            for _ in 0..count {
                if !criteria.insert(read_advancement_string(&mut input)?) {
                    return Err("duplicate datapack advancement criterion");
                }
            }
            if rows.insert(id, criteria).is_some() {
                return Err("duplicate datapack advancement");
            }
        }
        if datapack.insert(player, rows).is_some() {
            return Err("duplicate datapack advancement player");
        }
    }
    if !input.0.is_empty() {
        return Err("trailing advancement data");
    }
    Ok(SavedAdvancements { granted, core, datapack })
}

fn append_save_state(
    out: &mut Vec<u8>,
    players: &HashMap<[u8; 16], PlayerSave>,
    spawn_pitch: f32,
    mobs: &[Arc<Mob>],
    mob_data: &[u8],
) {
    let raids = saved_raids().lock().unwrap_or_else(|e| e.into_inner());
    let start = out.len();
    save_numbers!(out, spawn_pitch);
    let mut players: Vec<_> = players.iter().collect();
    players.sort_unstable_by_key(|&(&uuid, _)| uuid);
    write_varint(out, players.len() as i64);
    for (uuid, state) in players {
        out.extend_from_slice(uuid);
        state.write(out);
    }
    write_varint(out, mobs.len() as i64);
    out.extend_from_slice(mob_data);
    write_varint(out, raids.len() as i64);
    for (dim, raid) in raids.iter() {
        write_saved_raid(out, *dim, raid);
    }
    // SVX1 is inside SVP1, after the original records. Old SVP1 readers in
    // this version can still load older SVP1 bodies with no SVX1 suffix.
    out.extend_from_slice(SAVE_ENTITY_RULES_MAGIC);
    let rules = command_gamerules().lock().unwrap_or_else(|e| e.into_inner());
    write_varint(out, COMMAND_GAMERULE_DEFAULTS.len() as i64);
    for &(name, _, _) in COMMAND_GAMERULE_DEFAULTS.iter() {
        write_varint(out, name.len() as i64);
        out.extend_from_slice(name.as_bytes());
        save_numbers!(out, gamerule_value(name, &rules));
    }
    drop(rules);
    // EIDs change across loads, so persist custom mob attributes by MET4
    // ordinal and restore them against the newly assigned IDs.
    let attributes = attrs().lock().unwrap_or_else(|e| e.into_inner());
    let mut mob_attrs = Vec::new();
    for (index, mob) in mobs.iter().enumerate() {
        for (selector_key, key) in [
            (false, format!("e:{}", mob.eid)),
            (true, format!("e:m:{}", mob.eid)),
        ] {
            if let Some(values) = attributes.get(&key).filter(|values| !values.is_empty()) {
                mob_attrs.push((index, selector_key, values));
            }
        }
    }
    write_varint(out, mob_attrs.len() as i64);
    for (index, selector_key, values) in mob_attrs {
        write_varint(out, index as i64);
        out.push(u8::from(selector_key));
        let mut entries: Vec<_> = values.iter().collect();
        entries.sort_unstable_by_key(|(name, _)| *name);
        write_varint(out, entries.len() as i64);
        for (name, value) in entries {
            write_varint(out, name.len() as i64);
            out.extend_from_slice(name.as_bytes());
            save_numbers!(out, value);
        }
    }
    let size = (out.len() - start) as u64;
    save_numbers!(out, size);
    out.extend_from_slice(SAVE_STATE_MAGIC);
}

#[derive(Default)]
struct SaveState {
    spawn_pitch: f32,
    players: HashMap<[u8; 16], PlayerSave>,
    mobs: Vec<MobSave>,
    raids: Vec<(DimId, Raid)>,
    gamerules: Option<Vec<i32>>,
    mob_attributes: Vec<(usize, bool, HashMap<String, f64>)>,
}

impl SaveState {
    fn read(body: &[u8]) -> Result<Self, &'static str> {
        let mut input = SaveInput(body);
        let spawn_pitch = input.f32()?;
        let mut players = HashMap::new();
        for _ in 0..input.var_count(16)? {
            let uuid = input.take()?;
            if players
                .insert(uuid, PlayerSave::read(&mut input)?)
                .is_some()
            {
                return Err("duplicate saved player UUID");
            }
        }
        let mob_count = input.var_count(60)?;
        if mob_count > MOB_CAP {
            return Err("too many saved mobs");
        }
        let mut mobs = Vec::new();
        for _ in 0..mob_count {
            mobs.push(MobSave::read(&mut input)?);
        }
        let mut raids = Vec::new();
        for _ in 0..input.var_count(38)? {
            raids.push(read_saved_raid(&mut input)?);
        }
        let mut gamerules = None;
        let mut mob_attributes = Vec::new();
        if !input.0.is_empty() {
            if input.take::<4>()? != *SAVE_ENTITY_RULES_MAGIC {
                return Err("unknown save state extension");
            }
            let count = input.var_count(5)?;
            if count != COMMAND_GAMERULE_DEFAULTS.len() {
                return Err("invalid saved gamerule count");
            }
            let mut values = Vec::with_capacity(count);
            for &(name, is_bool, _) in COMMAND_GAMERULE_DEFAULTS.iter() {
                let len = input.var_count(1)?;
                if input.bytes(len)? != name.as_bytes() {
                    return Err("saved gamerule name mismatch");
                }
                let value = i32::from_be_bytes(input.take()?);
                if is_bool && !(0..=1).contains(&value) {
                    return Err("invalid saved boolean gamerule");
                }
                values.push(value);
            }
            gamerules = Some(values);
            let mut used = std::collections::HashSet::new();
            for _ in 0..input.var_count(3)? {
                let index = usize::try_from(input.varint()?).map_err(|_| "invalid saved mob ordinal")?;
                let selector_key = input.flag()?;
                if index >= mobs.len() || !used.insert((index, selector_key)) {
                    return Err("invalid or duplicate saved mob ordinal");
                }
                let mut entries = HashMap::new();
                for _ in 0..input.var_count(9)? {
                    let len = input.var_count(1)?;
                    let key = std::str::from_utf8(input.bytes(len)?)
                        .map_err(|_| "invalid saved mob attribute name")?.to_string();
                    if key.is_empty() || entries.insert(key, input.f64()?).is_some() {
                        return Err("invalid or duplicate saved mob attribute");
                    }
                }
                mob_attributes.push((index, selector_key, entries));
            }
        }
        if !input.0.is_empty() {
            return Err("trailing bytes in save state");
        }
        Ok(Self {
            spawn_pitch,
            players,
            mobs,
            raids,
            gamerules,
            mob_attributes,
        })
    }
}

fn rebuild_derived_indexes() {
    // Restart continuity: waters/hoppers/crops/plugin-cells are indexes OF
    // block state — a restart must rebuild them from the overlay exactly
    // as set_block would have registered them, or a reboot silently
    // freezes every machine vanilla would keep running.
    let owned = GENERATED_FLUID_OWNER
        .get()
        .map(|owner| owner.lock().unwrap_or_else(|e| e.into_inner()).owned.clone())
        .unwrap_or_default();
    let w = world().lock().unwrap_or_else(|e| e.into_inner());
    let mut wtr = waters().lock().unwrap_or_else(|e| e.into_inner());
    let mut hh = hoppers().lock().unwrap_or_else(|e| e.into_inner());
    let mut cg = crops().lock().unwrap_or_else(|e| e.into_inner());
    let mut cells = plugin_cells().lock().unwrap_or_else(|e| e.into_inner());
    let lamp_on = !plugins().lock().unwrap_or_else(|e| e.into_inner()).is_empty();
    // The indexes below hold positions, not `(dimension, position)`, so they describe the
    // Overworld: `rebuild_derived_indexes` walks the Overworld's edits and leaves another
    // dimension's alone until the indexes name a dimension themselves (S3).
    for (&(dim, cx, cz), c) in w.iter() {
        if dim != DimId::Overworld {
            continue;
        }
        for (idx, st) in c.iter() {
            let x = cx * 16 + (*idx & 15) as i32;
            let z = cz * 16 + ((*idx >> 4) & 15) as i32;
            let y = (*idx >> 8) as i32 + MIN_Y;
            if (80..=95).contains(st) {
                wtr.insert((x, y, z));
                if !owned.contains(&(x, y, z)) {
                    water_queue().lock().unwrap_or_else(|e| e.into_inner()).schedule((x, y, z), 1);
                }
            }
            if (96..=111).contains(st) {
                lavas().lock().unwrap_or_else(|e| e.into_inner()).insert((x, y, z));
                if !owned.contains(&(x, y, z)) {
                    lava_queue().lock().unwrap_or_else(|e| e.into_inner()).schedule((x, y, z), 1);
                }
            }
            if (9225..=9234).contains(st) {
                hh.insert((x, y, z));
                hopper_queue().lock().unwrap_or_else(|e| e.into_inner()).schedule((x, y, z), 1);
            }
            if (4278..=4285).contains(st) {
                cg.insert((DimId::Overworld, x, y, z), (*st - 4278) as u8);
            }
            if lamp_on && *st == ST_GROWTH_LAMP {
                cells
                    .entry((DimId::Overworld, cx, cz))
                    .or_default()
                    .push(PluginCell {
                    pi: 0,
                    x,
                    y,
                    z,
                    key: 0,
                    warm_ok: true,
                });
            }
        }
    }
    eprintln!(
        "[spine] rebuilt indexes: {} water, {} hoppers, {} crops, {} cells",
        wtr.len(),
        hh.len(),
        cg.len(),
        cells.len()
    );
}

// ---------- persistence (overlay only, no pregen) ----------
const WORLD_FILE: &str = "world_overlay.bin";
const WORLD_SEED_MAGIC: &[u8; 4] = b"SED1";
/// The companion overlay file of a dimension that is not the Overworld, or `WORLD_FILE` itself.
///
/// The legacy name and the legacy bytes stay the Overworld's: a server that only ever edits the
/// Overworld writes exactly the one file it always did, and none of the companions.
fn dim_overlay_name(dim: DimId) -> Option<&'static str> {
    match dim {
        DimId::Overworld => Some(WORLD_FILE),
        DimId::Nether => Some("world_overlay_nether.bin"),
        DimId::End => Some("world_overlay_end.bin"),
        // Custom dimensions keep their chunks inside `WORLD_FILE`'s DXT1 footer —
        // named by canonical dimension key, so adding or reordering datapack
        // dimensions never moves them — not a fixed companion file name.
        _ => None,
    }
}

fn dim_overlay_path(dir: &std::path::Path, dim: DimId) -> std::path::PathBuf {
    dir.join(dim_overlay_name(dim).unwrap_or(WORLD_FILE))
}

/// The directory the world save lives in: `WORLD_FILE`'s own, so the default bare file name means
/// the process's working directory — the same place the legacy save has always been read from.
fn world_save_dir() -> std::path::PathBuf {
    std::path::Path::new(WORLD_FILE)
        .parent()
        .map_or_else(std::path::PathBuf::new, std::path::Path::to_path_buf)
}

/// One dimension's overlay, as a save payload: `count`, then per chunk `cx, cz, count` and its
/// `idx, state` pairs. Returns how many chunks the table holds (0 = this dimension has no edits).
///
/// There is deliberately one writer for both the legacy file's leading table and a companion
/// file, and one reader for both, so "the Overworld's table" and "the same format for another
/// dimension" cannot drift into two formats.
fn save_dim_overlay_into(dim: DimId, f: &mut Vec<u8>) -> usize {
    let w = world().lock().unwrap_or_else(|e| e.into_inner());
    let chunks: Vec<(&DimChunkKey, &HashMap<u32, u32>)> =
        w.iter().filter(|(key, _)| key.0 == dim).collect();
    let count = chunks.len();
    write_varint(&mut *f, count as i64);
    for (key, c) in chunks {
        let (_, cx, cz) = *key;
        (*f).extend_from_slice(&cx.to_be_bytes());
        (*f).extend_from_slice(&cz.to_be_bytes());
        write_varint(&mut *f, c.len() as i64);
        for (&idx, &st) in c.iter() {
            (*f).extend_from_slice(&idx.to_be_bytes());
            write_varint(&mut *f, st as i64);
        }
    }
    count
}

/// Parse the chunk table into owned edits before applying any of it.
fn read_dim_overlay(
    dim: DimId,
    input: &mut SaveInput<'_>,
) -> Result<HashMap<DimChunkKey, HashMap<u32, u32>>, &'static str> {
    let count = input.var_count(9)?;
    let mut chunks = HashMap::new();
    for _ in 0..count {
        let cx = i32::from_be_bytes(input.take()?);
        let cz = i32::from_be_bytes(input.take()?);
        let count = input.var_count(5)?;
        let mut edits = HashMap::new();
        for _ in 0..count {
            let index = u32::from_be_bytes(input.take()?);
            let state = u32::try_from(input.varint()?).map_err(|_| "invalid overlay state")?;
            edits.insert(index, state);
        }
        chunks.insert((dim, cx, cz), edits);
    }
    Ok(chunks)
}

fn apply_dim_overlay(chunks: HashMap<DimChunkKey, HashMap<u32, u32>>) {
    let mut world = world().lock().unwrap();
    for (key @ (dim, cx, cz), edits) in chunks {
        world.entry(key).or_default().extend(edits);
        // Only Overworld edits can invalidate the Overworld band/section caches.
        if dim == DimId::Overworld {
            bump_world_epoch(cx << 4, cz << 4);
        }
    }
}

fn load_dim_overlay_from(dim: DimId, payload: &[u8]) -> Result<(), &'static str> {
    let mut input = SaveInput(payload);
    let chunks = read_dim_overlay(dim, &mut input)?;
    if !input.0.is_empty() {
        return Err("trailing bytes after a dimension's overlay table");
    }
    apply_dim_overlay(chunks);
    Ok(())
}

fn write_comps(f: &mut Vec<u8>, c: &Comps) {
    write_varint(f, c.len() as i64);
    for (ty, d) in c {
        write_varint(f, *ty);
        f.extend_from_slice(&(d.len() as u16).to_be_bytes());
        f.extend_from_slice(d);
    }
}
fn read_comps(b: &[u8], p: &mut usize) -> Comps {
    let n = rv_varint(b, p);
    let mut c = Vec::new();
    for _ in 0..n {
        // The type is a variable-width VarInt, so the 2-byte length field can only
        // be checked after it has been consumed. Checking first and then indexing
        // panicked on a truncated record.
        if *p >= b.len() {
            break;
        }
        let ty = rv_varint(b, p);
        // The VarInt itself may consume all remaining bytes.
        if b.len().saturating_sub(*p) < 2 {
            break;
        }
        let l = u16::from_be_bytes(b[*p..*p + 2].try_into().unwrap()) as usize;
        *p += 2;
        if b.len().saturating_sub(*p) < l {
            break;
        }
        c.push((ty, b[*p..*p + l].to_vec()));
        *p += l;
    }
    c
}
/// The one wire encoder for a container's slot table. The legacy chest section and the DIM2
/// footer both call this, so a future change to item components cannot make one format drift.
fn write_stack_table(out: &mut Vec<u8>, slots: &HashMap<i16, Stack>) {
    write_varint(out, slots.len() as i64);
    for (&slot, (item, cnt, comps)) in slots.iter() {
        out.extend_from_slice(&slot.to_be_bytes());
        write_varint(out, *item);
        out.push(*cnt);
        write_comps(out, comps);
    }
}
/// The matching decoder. `has_comps` is false for MET2 (the pre-component wire form); the lenient
/// `break` on truncation is the legacy loader's behaviour, and DIM2 adds strictness by requiring
/// that the footer's length-delimited body be consumed exactly.
fn read_stack_table(payload: &[u8], p: &mut usize, has_comps: bool) -> HashMap<i16, Stack> {
    let n = rv_varint(payload, p);
    let mut slots = HashMap::new();
    for _ in 0..n {
        if p.saturating_add(3) > payload.len() {
            break;
        }
        let slot = i16::from_be_bytes(payload[*p..*p + 2].try_into().unwrap());
        *p += 2;
        let item = rv_varint(payload, p);
        if *p >= payload.len() {
            break;
        }
        let cnt = payload[*p];
        *p += 1;
        let comps = if has_comps {
            read_comps(payload, p)
        } else {
            Vec::new()
        };
        slots.insert(slot, (item, cnt, comps));
    }
    slots
}
/// A byte-for-byte copy of the fields the footer serialises. The caches are deliberately fresh:
/// they are derived state and are rebuilt on load, never persisted.
fn clone_furnace_for_footer(fr: &Furnace) -> Furnace {
    Furnace {
        input: fr.input.clone(),
        fuel: fr.fuel.clone(),
        output: fr.output.clone(),
        burn_left: fr.burn_left,
        progress: fr.progress,
        dirty: false,
        xp: 0.0,
        recipe_cache: CachedLookup::default(),
        fuel_cache: CachedLookup::default(),
    }
}
/// One Furnace's three stacks and two counters, in the legacy furnace-section layout.
fn write_furnace_state(out: &mut Vec<u8>, fr: &Furnace) {
    write_varint(out, fr.input.0);
    out.push(fr.input.1);
    write_comps(out, &fr.input.2);
    write_varint(out, fr.fuel.0);
    out.push(fr.fuel.1);
    write_comps(out, &fr.fuel.2);
    write_varint(out, fr.output.0);
    out.push(fr.output.1);
    write_comps(out, &fr.output.2);
    out.extend_from_slice(&fr.burn_left.to_be_bytes());
    out.extend_from_slice(&fr.progress.to_be_bytes());
}
fn read_furnace_state(
    payload: &[u8],
    p: &mut usize,
    has_comps: bool,
    legacy_ms_ages: bool,
) -> Option<Furnace> {
    let rd = |p: &mut usize| -> Stack {
        let it = rv_varint(payload, p);
        let c = if *p < payload.len() { payload[*p] } else { 0 };
        *p += 1;
        let comps = if has_comps {
            read_comps(payload, p)
        } else {
            Vec::new()
        };
        (it, c, comps)
    };
    let input = rd(p);
    let fuel = rd(p);
    let output = rd(p);
    if p.saturating_add(8) > payload.len() {
        return None;
    }
    let burn_left = legacy_tick_count(
        i32::from_be_bytes(payload[*p..*p + 4].try_into().unwrap()) as u64,
        legacy_ms_ages,
    ) as i32;
    *p += 4;
    let progress = legacy_tick_count(
        i32::from_be_bytes(payload[*p..*p + 4].try_into().unwrap()) as u64,
        legacy_ms_ages,
    ) as i32;
    *p += 4;
    Some(Furnace {
        input,
        fuel,
        output,
        burn_left,
        progress,
        dirty: false,
        xp: 0.0,
        recipe_cache: CachedLookup::default(),
        fuel_cache: CachedLookup::default(),
    })
}
fn rv_varint(b: &[u8], p: &mut usize) -> i64 {
    let mut v = 0i64;
    let mut sh = 0;
    loop {
        if *p >= b.len() {
            break;
        }
        let x = b[*p];
        *p += 1;
        v |= ((x & 0x7f) as i64) << sh;
        if x & 0x80 == 0 {
            break;
        }
        sh += 7;
        if sh > 63 {
            break;
        }
    }
    v
}
struct SaveInput<'a>(&'a [u8]);

impl<'a> SaveInput<'a> {
    fn bytes(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
        let (value, rest) = self.0.split_at_checked(len).ok_or("truncated save data")?;
        self.0 = rest;
        Ok(value)
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], &'static str> {
        Ok(self.bytes(N)?.try_into().unwrap())
    }

    fn byte(&mut self) -> Result<u8, &'static str> {
        Ok(self.take::<1>()?[0])
    }

    fn varint(&mut self) -> Result<i64, &'static str> {
        read_varint(&mut self.0).ok_or("invalid save VarInt")
    }

    fn var_count(&mut self, min_entry_size: usize) -> Result<usize, &'static str> {
        let count = usize::try_from(self.varint()?).map_err(|_| "invalid save count")?;
        if count > self.0.len() / min_entry_size {
            return Err("invalid save count");
        }
        Ok(count)
    }
    fn flag(&mut self) -> Result<bool, &'static str> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("invalid save flag"),
        }
    }

    fn dim(&mut self) -> Result<DimId, &'static str> {
        DimId::from_index(usize::from(self.byte()?)).ok_or("invalid save dimension")
    }

    fn f32(&mut self) -> Result<f32, &'static str> {
        let value = f32::from_be_bytes(self.take()?);
        value
            .is_finite()
            .then_some(value)
            .ok_or("non-finite save float")
    }

    fn f64(&mut self) -> Result<f64, &'static str> {
        let value = f64::from_be_bytes(self.take()?);
        value
            .is_finite()
            .then_some(value)
            .ok_or("non-finite save double")
    }

    fn position(&mut self) -> Result<(f64, f64, f64), &'static str> {
        Ok((self.f64()?, self.f64()?, self.f64()?))
    }

    fn slots(&mut self, range: std::ops::Range<i16>) -> Result<HashMap<i16, Stack>, &'static str> {
        let count = self.var_count(5)?;
        if count > range.len() {
            return Err("too many saved slots");
        }
        let mut slots = HashMap::new();
        for _ in 0..count {
            let slot = i16::from_be_bytes(self.take()?);
            if !range.contains(&slot) || slots.insert(slot, self.stack()?).is_some() {
                return Err("invalid or duplicate saved slot");
            }
        }
        Ok(slots)
    }

    fn stack(&mut self) -> Result<Stack, &'static str> {
        let item = self.varint()?;
        let count = self.byte()?;
        if item <= 0 || count == 0 {
            return Err("invalid saved item stack");
        }
        Ok((item, count, self.components()?))
    }

    fn components(&mut self) -> Result<Comps, &'static str> {
        let count = self.var_count(2)?;
        let mut components = Vec::new();
        for _ in 0..count {
            let ty = self.varint()?;
            let length = usize::try_from(self.varint()?).map_err(|_| "invalid component length")?;
            components.push((ty, self.bytes(length)?.to_vec()));
        }
        Ok(components)
    }

    fn optional_uuid(&mut self) -> Result<Option<[u8; 16]>, &'static str> {
        self.flag()?.then(|| self.take()).transpose()
    }

    fn block_pos(&mut self) -> Result<BlockPos, &'static str> {
        Ok((
            i32::from_be_bytes(self.take()?),
            i32::from_be_bytes(self.take()?),
            i32::from_be_bytes(self.take()?),
        ))
    }

    fn optional_block_pos(&mut self) -> Result<Option<BlockPos>, &'static str> {
        self.flag()?.then(|| self.block_pos()).transpose()
    }

    fn optional_tick(&mut self) -> Result<Option<u64>, &'static str> {
        self.flag()?
            .then(|| self.take().map(u64::from_be_bytes))
            .transpose()
    }
}

// Envelope protects new saves from silent partial-load and overwrite after a
// truncated or torn write. Legacy MET2/MET3 files remain readable for migration.
// SHA-1 detects accidental damage; it does not authenticate untrusted files.
const SAVE_MAGIC: &[u8; 4] = b"SPN4";
fn wrap_world_payload(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 32);
    out.extend_from_slice(SAVE_MAGIC);
    out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    out.extend_from_slice(&sha1(payload));
    out.extend_from_slice(payload);
    out
}
fn unwrap_world_payload(raw: Vec<u8>) -> Result<Vec<u8>, &'static str> {
    if raw.starts_with(SAVE_MAGIC) {
        if raw.len() < 32 {
            return Err("truncated save header");
        }
        let len = u64::from_be_bytes(raw[4..12].try_into().unwrap());
        if len != (raw.len() - 32) as u64 {
            return Err("save length mismatch");
        }
        if sha1(&raw[32..]) != raw[12..32] {
            return Err("save checksum mismatch");
        }
        return Ok(raw[32..].to_vec());
    }
    if raw.starts_with(b"SPN") {
        return Err("unsupported or corrupted save format");
    }
    Ok(raw) // existing unwrapped v2.37 format
}
// Separate ownership transitions from file writers. Writers serialize before
// taking their snapshots so an older snapshot cannot overtake a newer one.
static SAVE_LOCK: Mutex<()> = Mutex::new(());
static SAVE_WRITER: Mutex<()> = Mutex::new(());
thread_local! { static SAVE_GUARD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) } }
/// Run a world/entity ownership transition under `SAVE_LOCK`, so the signal-thread save cannot
/// observe the block removed but the entity not yet added, or a landing placed but its entity
/// still queued. Re-entrant per thread: `spawn_fall` is reached from `falling_tick`'s placement
/// and from another `spawn_fall`, and only the outermost call locks.
fn save_lock_prof() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("MEGU_LOCK_PROF").is_some())
}
#[track_caller]
fn with_save_guard<R>(f: impl FnOnce() -> R) -> R {
    if SAVE_GUARD.with(|g| g.get()) {
        return f();
    }
    let prof = save_lock_prof();
    let __wt = std::time::Instant::now();
    let _save = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if prof {
        let __w = __wt.elapsed();
        if __w > std::time::Duration::from_millis(20) {
            eprintln!("[save-lock] wait {:?} at {}", __w, std::panic::Location::caller());
        }
    }
    let __ht = std::time::Instant::now();
    struct HoldLog(std::time::Instant, &'static std::panic::Location<'static>, bool);
    impl std::ops::Drop for HoldLog {
        fn drop(&mut self) {
            if !self.2 {
                return;
            }
            let h = self.0.elapsed();
            if h > std::time::Duration::from_millis(20) {
                eprintln!("[save-lock] held {:?} at {}", h, self.1);
            }
        }
    }
    let _hl = HoldLog(__ht, std::panic::Location::caller(), prof);
    SAVE_GUARD.with(|g| g.set(true));
    struct Reset;
    impl std::ops::Drop for Reset {
        fn drop(&mut self) {
            SAVE_GUARD.with(|g| g.set(false));
        }
    }
    let _reset = Reset;
    f()
}
fn persistence_boundary<T>(
    message: &'static str,
    run: impl FnOnce() -> Result<T, &'static str>,
) -> Result<T, &'static str> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).map_err(|_| message)?
}

fn save_world() {
    save_world_guarded_in(&world_save_dir());
}

fn save_world_guarded_in(dir: &std::path::Path) {
    if let Err(error) = save_world_guarded(|snapshot| snapshot.write(dir)) {
        eprintln!("[spine] WORLD SAVE FAILED: {error}");
    }
}

/// Only capture needs the world transition guard. The writer flushes owned
/// bytes without blocking other threads that need the world guard.
fn save_world_guarded(
    write: impl FnOnce(WorldSaveSnapshot) -> Result<(), String>,
) -> Result<(), String> {
    if SAVE_GUARD.with(|guard| guard.get()) {
        return Err("world save called inside an ownership transition".into());
    }
    let _writer = SAVE_WRITER
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let snapshot = with_save_guard(|| {
        persistence_boundary("world snapshot panicked", || {
            Ok(WorldSaveSnapshot::capture())
        })
    })?;
    write(snapshot)
}

struct WorldSaveSnapshot {
    overworld: Vec<u8>,
    companions: [(DimId, Option<Vec<u8>>); 2],
}

impl WorldSaveSnapshot {
    fn capture() -> Self {
        let mut overworld = Vec::new();
        save_world_into(&mut overworld);
        let companions = [DimId::Nether, DimId::End].map(|dim| {
            let mut payload = Vec::new();
            let count = save_dim_overlay_into(dim, &mut payload);
            (dim, (count != 0).then_some(payload))
        });
        Self {
            overworld,
            companions,
        }
    }

    fn write(self, dir: &std::path::Path) -> Result<(), String> {
        let interval = connection_limits::backup_interval();
        write_world_file(
            &dim_overlay_path(dir, DimId::Overworld),
            &wrap_world_payload(&self.overworld),
            interval,
        )?;
        // Attempt both companions even if either one fails. Empty dimensions
        // drop their old overlays so a restart cannot resurrect removed edits.
        let results = self.companions.map(|(dim, payload)| {
            let path = dim_overlay_path(dir, dim);
            match payload {
                Some(payload) => write_world_file(&path, &wrap_world_payload(&payload), interval),
                None => remove_world_save(&path),
            }
        });
        results.into_iter().collect()
    }
}

/// Remove a save and every sibling `write_world_file` may have left beside it.
fn remove_world_save(path: &std::path::Path) -> Result<(), String> {
    for suffix in ["", ".tmp", ".bak", ".bak.1", ".bak.2"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let target = std::path::PathBuf::from(name);
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot remove {}: {error}", target.display())),
        }
    }
    Ok(())
}
/// Write `framed` to `path` via `path.tmp` (fsynced), then rename over it.
///
/// Backups are `path.bak` (the previous save, so at most one autosave old),
/// `path.bak.1` and `path.bak.2`. The older two only advance when `bak.1` is at
/// least `interval` old, so three generations span more than a few autosaves.
/// Every failure is returned with the path involved; the live file is only
/// replaced after the tmp file is complete.
fn write_world_file(
    path: &std::path::Path,
    framed: &[u8],
    interval: Duration,
) -> Result<(), String> {
    let with = |suffix: &str| {
        let mut o = path.as_os_str().to_owned();
        o.push(suffix);
        std::path::PathBuf::from(o)
    };
    let (tmp, bak, bak1, bak2) = (with(".tmp"), with(".bak"), with(".bak.1"), with(".bak.2"));
    {
        let mut f = std::fs::File::create(&tmp)
            .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        if let Err(error) = f.write_all(framed).and_then(|_| f.sync_all()) {
            drop(f);
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("cannot write {}: {error}", tmp.display()));
        }
    }
    if path.exists() {
        // Backup trouble is reported but never blocks the save itself.
        let age = |p: &std::path::Path| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
        };
        if bak.exists() && age(&bak1).is_none_or(|a| a >= interval) {
            if bak1.exists() {
                if let Err(e) = std::fs::rename(&bak1, &bak2) {
                    eprintln!("[spine] backup rotate {} failed: {e}", bak1.display());
                }
            }
            if let Err(e) = std::fs::copy(&bak, &bak1) {
                eprintln!("[spine] backup rotate {} failed: {e}", bak.display());
            }
        }
        if let Err(e) = std::fs::copy(path, &bak) {
            eprintln!("[spine] backup {} failed: {e}", bak.display());
        }
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!(
            "cannot rename {} over {}: {e}",
            tmp.display(),
            path.display()
        )
    })
}
// Caller owns SAVE_LOCK (or is a single-threaded fixture). Some existing
// callers hold the mutex directly, without the re-entrant thread-local guard.
fn save_world_into(f: &mut Vec<u8>) {
    let world_spawn = saved_value(&WORLD_SPAWN);
    let spawn_pitch = saved_value(&WORLD_SPAWN_PITCH);
    let (pending, sources, spider_flags) = worldgen::with_gen(|g| (
        g.take_staged_payloads(), g.payload_sources.clone(), g.mineshaft_spawners.clone()
    )).unwrap_or_default();
    materialize_generation_payloads_inner(pending);
    let mut content = generated_content().lock().unwrap_or_else(|e| e.into_inner()).clone();
    {
        let ch = chests().lock().unwrap_or_else(|e| e.into_inner());
        for cart in &mut content.carts {
            if !cart.has_loot_table() {
                if let Ok(uuid) = cart.uuid() {
                    let slots = ch.get(&ContainerKey::Cart(uuid)).cloned().unwrap_or_default();
                    if let crate::nbt::Tag::Compound(fields) = &mut cart.data {
                        fields.insert(crate::nbt::NbtString::text("Items"), stacks_to_items_nbt(&slots));
                    }
                }
            }
        }
    }
    let generated = generated_content::Snapshot {
        content, sources, spider_flags,
    };
    let active_players: Vec<_> = client_snapshot()
        .into_iter()
        .map(|client| {
            let position = client_dim_pos(&client);
            let inventory = client.inv.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let state = PlayerSave::capture(&client, position, &inventory);
            (client.uuid, client.name.clone(), inventory, state)
        })
        .collect();
    let mut players = saved_players().lock().unwrap_or_else(|e| e.into_inner()).clone();
    for (uuid, _, _, state) in &active_players {
        players.insert(*uuid, state.clone());
    }
    // The leading chunk table is the Overworld's, which is what the legacy file has always held:
    // its count, its keys and its bytes are unchanged, and another dimension's edits go to that
    // dimension's own companion file (`save_world_guarded_in`).
    save_dim_overlay_into(DimId::Overworld, f);
    // META section: mobs + world time + inventories (offline store + live).
    // MET4 = stacks carry component blobs and entity ages are server ticks.
    // MET2/MET3 stored those ages in milliseconds, which the loader converts.
    let mut drop_owners: Vec<DropOwnerRecord> = Vec::new();
    // Non-Overworld signs and player dimensions ride in a footer (see `append_dim_footer`), so the
    // legacy sections below stay byte-for-byte identical for an Overworld-only save.
    let dim_signs: Vec<DimSignRecord>;
    let mut dim_players: Vec<DimPlayerRecord> = Vec::new();
    // Chests, furnaces, crops and pending loot outside the Overworld go to their own footer; the
    // legacy sections below carry the Overworld's entries in the old wire layout. Crops are
    // collected before any container lock: the growth tick holds `crops` while writing a block,
    // which can take `chests`, so taking them in the opposite order here would invert the locks.
    let mut dim2 = Dim2Entries::default();
    let mut dxt = DxtEntries::default();
    for (pos, age) in crops().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if pos.0 == DimId::Overworld {
            continue;
        }
        if pos.0.is_vanilla() {
            dim2.crops.push((*pos, *age));
        } else {
            dxt.crops.push((dim_key(pos.0), pos.1, pos.2, pos.3, *age));
        }
    }
    (*f).extend_from_slice(b"MET4");
    let ms = mobs().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mob_dimensions = ms.iter().map(|mob| mob.dim).collect::<Vec<_>>();
    let mut mob_data = Vec::new();
    {
        let now = TICK.load(Ordering::Relaxed);
        write_varint(&mut *f, ms.len() as i64);
        for m in &ms {
            let (x, y, z, yaw, pitch) = saved_value(&m.pos);
            write_varint(&mut *f, m.etype);
            (*f).extend_from_slice(&x.to_be_bytes());
            (*f).extend_from_slice(&y.to_be_bytes());
            (*f).extend_from_slice(&z.to_be_bytes());
            (*f).extend_from_slice(
                &{
                    let h = m.hp.lock().unwrap_or_else(|e| e.into_inner());
                    *h
                }
                .to_be_bytes(),
            );
            MobSave::write(&mut mob_data, m, yaw, pitch, now);
        }
    }
    (*f).extend_from_slice(&WORLD_TIME.load(Ordering::Relaxed).to_be_bytes());
    {
        let mut all: HashMap<String, HashMap<i16, Stack>> = inv_store().lock().unwrap_or_else(|e| e.into_inner()).clone();
        for (_, name, slots, _) in &active_players {
            all.insert(name.clone(), slots.clone());
        }
        write_varint(&mut *f, all.len() as i64);
        for (name, inv) in all.iter() {
            (*f).extend_from_slice(&(name.len() as u16).to_be_bytes());
            (*f).extend_from_slice(name.as_bytes());
            write_varint(&mut *f, inv.len() as i64);
            for (&slot, (item, cnt, ic)) in inv.iter() {
                (*f).extend_from_slice(&slot.to_be_bytes());
                write_varint(&mut *f, *item);
                (*f).push(*cnt);
                write_varint(&mut *f, ic.len() as i64);
                for (ty, d) in ic {
                    write_varint(&mut *f, *ty);
                    (*f).extend_from_slice(&(d.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(d);
                }
            }
        }
        let ch = chests().lock().unwrap_or_else(|e| e.into_inner());
        // The legacy chest table holds the Overworld's block containers only — including the
        // synthetic per-player Ender keys, which always live under the Overworld — in the exact
        // old `(x, y, z)` wire layout. Anything in another dimension goes to DIM2.
        let ow_chests: Vec<_> = ch
            .iter()
            .filter_map(|(key, slots)| match key {
                ContainerKey::Block((DimId::Overworld, x, y, z)) => Some(((*x, *y, *z), slots)),
                ContainerKey::Block(pos) => {
                    if pos.0.is_vanilla() {
                        dim2.chests.push((*pos, slots.clone()));
                    } else {
                        dxt.chests
                            .push((dim_key(pos.0), pos.1, pos.2, pos.3, slots.clone()));
                    }
                    None
                }
                ContainerKey::Cart(_) => None,
            })
            .collect();
        write_varint(&mut *f, ow_chests.len() as i64);
        for ((x, y, z), slots) in ow_chests {
            (*f).extend_from_slice(&x.to_be_bytes());
            (*f).extend_from_slice(&y.to_be_bytes());
            (*f).extend_from_slice(&z.to_be_bytes());
            write_stack_table(&mut *f, slots);
        }
        let sg = signs().lock().unwrap_or_else(|e| e.into_inner());
        // The legacy signs section carries the Overworld's signs only, in the exact old wire
        // layout. Another dimension's sign goes to the footer, so its presence cannot shift a
        // single byte of an Overworld-only save.
        let ow_signs: Vec<_> = sg.iter().filter(|(k, _)| k.0 == DimId::Overworld).collect();
        write_varint(&mut *f, ow_signs.len() as i64);
        for (k, lines) in &ow_signs {
            let &(_dim, x, y, z, front) = *k;
            (*f).extend_from_slice(&x.to_be_bytes());
            (*f).extend_from_slice(&y.to_be_bytes());
            (*f).extend_from_slice(&z.to_be_bytes());
            (*f).push(front);
            for l in lines.iter() {
                (*f).extend_from_slice(&(l.len() as u16).to_be_bytes());
                (*f).extend_from_slice(l.as_bytes());
            }
        }
        dim_signs = sg
            .iter()
            .filter(|(k, _)| k.0 != DimId::Overworld && k.0.is_vanilla())
            .map(|(k, v)| (k.0, k.1, k.2, k.3, k.4, v.clone()))
            .collect();
        for (k, v) in sg.iter().filter(|(k, _)| !k.0.is_vanilla()) {
            dxt.signs
                .push((dim_key(k.0), k.1, k.2, k.3, k.4, v.clone()));
        }
        // forceload tail — vanilla keeps forced chunks across restarts;
        // losing them would silently freeze fixture+machine regions.
        let fl = forceloaded().lock().unwrap_or_else(|e| e.into_inner());
        write_varint(&mut *f, fl.len() as i64);
        for &(cx, cz) in fl.iter() {
            (*f).extend_from_slice(&cx.to_be_bytes());
            (*f).extend_from_slice(&cz.to_be_bytes());
        }
        // Players log back in where they left. The legacy six fields are
        // written for every player (so an Overworld-only save is byte-identical); the dimension of
        // a player who logged out elsewhere is corrected from the footer on load.
        let mut allp: HashMap<String, DimPosRot> = pos_store().lock().unwrap_or_else(|e| e.into_inner()).clone();
        for (_, name, _, state) in &active_players {
            allp.insert(name.clone(), state.position);
        }
        write_varint(&mut *f, allp.len() as i64);
        for (name, &(dim, x, y, z, yaw, pitch)) in allp.iter() {
            (*f).extend_from_slice(&(name.len() as u16).to_be_bytes());
            (*f).extend_from_slice(name.as_bytes());
            (*f).extend_from_slice(&x.to_be_bytes());
            (*f).extend_from_slice(&y.to_be_bytes());
            (*f).extend_from_slice(&z.to_be_bytes());
            (*f).extend_from_slice(&yaw.to_be_bytes());
            (*f).extend_from_slice(&pitch.to_be_bytes());
            if dim != DimId::Overworld {
                if dim.is_vanilla() {
                    dim_players.push((name.clone(), dim));
                } else {
                    dxt.players.push((name.clone(), dim_key(dim)));
                }
            }
        }
        // dropped items are world entities in vanilla — they survive
        // restart inside their despawn budget
        // A trident that came to rest is persisted as a ground drop. Saving must be
        // observational: the conversion is written into the payload instead of
        // being applied to the live world, where it also lost the new drop because
        // the drop list had already been serialized.
        let tridents: Vec<(f64, f64, f64, i64, Comps)> = arrows()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|a| a.kind == 1 && a.ret_item > 0)
            .map(|a| (a.x, a.y, a.z, a.ret_item, a.ret_comps.clone()))
            .collect();
        let mut drop_ages: Vec<u32> = Vec::new();
        {
            let ds = drops().lock().unwrap_or_else(|e| e.into_inner());
            write_varint(&mut *f, (ds.len() + tridents.len()) as i64);
            for (idx, d) in ds.iter().enumerate() {
                (*f).extend_from_slice(&d.eid.to_be_bytes());
                (*f).extend_from_slice(&d.x.to_be_bytes());
                (*f).extend_from_slice(&d.y.to_be_bytes());
                (*f).extend_from_slice(&d.z.to_be_bytes());
                write_varint(&mut *f, d.item);
                (*f).push(d.cnt);
                write_comps(&mut *f, &d.comps);
                // `as u32` is already a bit-preserving two's complement cast, so a
                // negative pickup-delay marker round-trips; only the load-side
                // clamp used to destroy it.
                drop_ages.push(d.age as u32); // server ticks, signed
                if d.target.is_some() || d.thrower.is_some() {
                    drop_owners.push((idx as u32, d.target, d.thrower));
                }
            }
            for (x, y, z, item, comps) in tridents.iter() {
                // eid 0 is remapped on load and age 0 grants a full pickup delay.
                (*f).extend_from_slice(&0i32.to_be_bytes());
                (*f).extend_from_slice(&x.to_be_bytes());
                (*f).extend_from_slice(&y.to_be_bytes());
                (*f).extend_from_slice(&z.to_be_bytes());
                write_varint(&mut *f, *item);
                (*f).push(1);
                write_comps(&mut *f, comps);
                drop_ages.push(0);
            }
        }
        // Furnaces are block entities — contents and smelting state are durable in vanilla. The
        // legacy furnace table holds the Overworld's entries in the old wire layout; another
        // dimension's furnace goes to DIM2.
        let fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
        let ow_furnaces: Vec<_> = fs
            .iter()
            .filter_map(|(pos, fr)| match pos.0 {
                DimId::Overworld => Some((pos, fr)),
                dim if dim.is_vanilla() => {
                    dim2.furnaces.push((*pos, clone_furnace_for_footer(fr)));
                    None
                }
                dim => {
                    dxt.furnaces.push((
                        dim_key(dim),
                        pos.1,
                        pos.2,
                        pos.3,
                        clone_furnace_for_footer(fr),
                    ));
                    None
                }
            })
            .collect();
        write_varint(&mut *f, ow_furnaces.len() as i64);
        for ((_, x, y, z), fr) in ow_furnaces {
            (*f).extend_from_slice(&x.to_be_bytes());
            (*f).extend_from_slice(&y.to_be_bytes());
            (*f).extend_from_slice(&z.to_be_bytes());
            write_furnace_state(&mut *f, fr);
        }
        // sheared-mob indices (eids are re-issued on load — persist by
        // mob order, not eid)
        {
            let sh = sheared().lock().unwrap_or_else(|e| e.into_inner());
            let idx: Vec<i64> = ms
                .iter()
                .enumerate()
                .filter(|(_, m)| sh.contains(&m.eid))
                .map(|(i, _)| i as i64)
                .collect();
            write_varint(&mut *f, idx.len() as i64);
            for i in idx {
                write_varint(&mut *f, i);
            }
        }
        // Keep the legacy scoreboard layout. Both pieces describe one snapshot.
        let (scoreboard_objectives, scoreboard_sidebar) = {
            let board = scoreboard().lock().unwrap_or_else(|e| e.into_inner());
            write_varint(&mut *f, board.objectives.len() as i64);
            for obj in board.objectives.keys() {
                (*f).extend_from_slice(&(obj.len() as u16).to_be_bytes());
                (*f).extend_from_slice(obj.as_bytes());
                let entries = board.scores.get(obj);
                write_varint(&mut *f, entries.map_or(0, HashMap::len) as i64);
                for (entry, score) in entries.into_iter().flatten() {
                    (*f).extend_from_slice(&(entry.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(entry.as_bytes());
                    (*f).extend_from_slice(&i64::from(score.value).to_be_bytes());
                }
            }
            (
                board.objectives.values().map(|obj| (obj.name.clone(), obj.criteria)).collect::<Vec<_>>(),
                board.display(ScoreboardDisplaySlot::Sidebar).unwrap_or_default().to_string(),
            )
        };
        // XP orbs are world entities too — same persistence class as drops
        {
            let xs = xps().lock().unwrap_or_else(|e| e.into_inner());
            write_varint(&mut *f, xs.len() as i64);
            for x in xs.iter() {
                (*f).extend_from_slice(&x.eid.to_be_bytes());
                (*f).extend_from_slice(&x.x.to_be_bytes());
                (*f).extend_from_slice(&x.y.to_be_bytes());
                (*f).extend_from_slice(&x.z.to_be_bytes());
                (*f).extend_from_slice(&x.val.to_be_bytes());
                (*f).extend_from_slice(&x.age.to_be_bytes());
            }
        }
        // drop despawn-budget residue: elapsed ms per drop, by order
        for a in drop_ages {
            (*f).extend_from_slice(&a.to_be_bytes());
        }
        // projectiles + primed tnt are world entities in vanilla —
        // they survive restart mid-flight
        {
            let ar = arrows().lock().unwrap_or_else(|e| e.into_inner());
            // ARW2: stuck flag persists so saved arrows resume embedded
            (*f).extend_from_slice(b"ARW2");
            // Tridents travel in the drop list above, so they are excluded here
            // rather than removed from the live arrow list.
            let persisted: Vec<&Arrow> = ar
                .iter()
                .filter(|a| !(a.kind == 1 && a.ret_item > 0))
                .collect();
            write_varint(&mut *f, persisted.len() as i64);
            for a in persisted {
                (*f).extend_from_slice(&a.eid.to_be_bytes());
                (*f).extend_from_slice(&a.owner.to_be_bytes());
                (*f).extend_from_slice(&a.x.to_be_bytes());
                (*f).extend_from_slice(&a.y.to_be_bytes());
                (*f).extend_from_slice(&a.z.to_be_bytes());
                (*f).extend_from_slice(&a.dx.to_be_bytes());
                (*f).extend_from_slice(&a.dy.to_be_bytes());
                (*f).extend_from_slice(&a.dz.to_be_bytes());
                (*f).extend_from_slice(&a.ttl.to_be_bytes());
                (*f).push(a.stuck as u8);
            }
            let bo = bobbers().lock().unwrap_or_else(|e| e.into_inner());
            write_varint(&mut *f, bo.len() as i64);
            for a in bo.iter() {
                (*f).extend_from_slice(&a.eid.to_be_bytes());
                (*f).extend_from_slice(&a.owner.to_be_bytes());
                (*f).extend_from_slice(&a.x.to_be_bytes());
                (*f).extend_from_slice(&a.y.to_be_bytes());
                (*f).extend_from_slice(&a.z.to_be_bytes());
                (*f).extend_from_slice(&a.dx.to_be_bytes());
                (*f).extend_from_slice(&a.dy.to_be_bytes());
                (*f).extend_from_slice(&a.dz.to_be_bytes());
                (*f).extend_from_slice(&a.age.to_be_bytes());
                (*f).push(a.bit as u8);
            }
            let pe = pearls().lock().unwrap_or_else(|e| e.into_inner());
            write_varint(&mut *f, pe.len() as i64);
            for a in pe.iter() {
                (*f).extend_from_slice(&a.eid.to_be_bytes());
                (*f).extend_from_slice(&a.owner.to_be_bytes());
                (*f).extend_from_slice(&a.x.to_be_bytes());
                (*f).extend_from_slice(&a.y.to_be_bytes());
                (*f).extend_from_slice(&a.z.to_be_bytes());
                (*f).extend_from_slice(&a.dx.to_be_bytes());
                (*f).extend_from_slice(&a.dy.to_be_bytes());
                (*f).extend_from_slice(&a.dz.to_be_bytes());
                (*f).extend_from_slice(&a.ttl.to_be_bytes());
            }
            let pr = primed().lock().unwrap_or_else(|e| e.into_inner());
            write_varint(&mut *f, pr.len() as i64);
            for a in pr.iter() {
                (*f).extend_from_slice(&a.eid.to_be_bytes());
                (*f).extend_from_slice(&a.x.to_be_bytes());
                (*f).extend_from_slice(&a.y.to_be_bytes());
                (*f).extend_from_slice(&a.z.to_be_bytes());
                (*f).extend_from_slice(&a.left.to_be_bytes());
            }
        }
        // weather is world state in vanilla — rain budget survives restart
        (*f).extend_from_slice(&RAINING.load(Ordering::Relaxed).to_be_bytes());
        (*f).extend_from_slice(&THUNDERING.load(Ordering::Relaxed).to_be_bytes());
        // gamerules are world state too (level.dat GameRules)
        for r in [
            &GR_KEEP_INVENTORY,
            &GR_DAYLIGHT_CYCLE,
            &GR_WEATHER_CYCLE,
            &GR_MOB_SPAWNING,
            &GR_MOB_GRIEFING,
            &GR_NATURAL_REGEN,
            &GR_MOB_LOOT,
        ] {
            (*f).push(r.load(Ordering::Relaxed));
        }
        (*f).extend_from_slice(&world_spawn.0.to_be_bytes());
        (*f).extend_from_slice(&world_spawn.1.to_be_bytes());
        (*f).extend_from_slice(&world_spawn.2.to_be_bytes());
        (*f).push(DIFFICULTY.load(Ordering::Relaxed));
        // entity tags: players by name, mobs by ordinal (eids re-issue)
        {
            let tm = tags().lock().unwrap_or_else(|e| e.into_inner());
            let players: Vec<(&String, &HashSet<String>)> =
                tm.iter().filter(|(k, _)| k.starts_with("p:")).collect();
            write_varint(&mut *f, players.len() as i64);
            for (k, st) in players {
                let nm = &k[2..];
                (*f).extend_from_slice(&(nm.len() as u16).to_be_bytes());
                (*f).extend_from_slice(nm.as_bytes());
                write_varint(&mut *f, st.len() as i64);
                for t in st.iter() {
                    (*f).extend_from_slice(&(t.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(t.as_bytes());
                }
            }
            let mobe: Vec<(i64, &HashSet<String>)> = ms
                .iter()
                .enumerate()
                .filter_map(|(i, m)| tm.get(&format!("e:{}", m.eid)).map(|st| (i as i64, st)))
                .collect();
            write_varint(&mut *f, mobe.len() as i64);
            for (i, st) in mobe {
                write_varint(&mut *f, i);
                write_varint(&mut *f, st.len() as i64);
                for t in st.iter() {
                    (*f).extend_from_slice(&(t.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(t.as_bytes());
                }
            }
        }
        // scheduled functions: stored as REMAINING ticks (TICK is not
        // persisted — re-anchor on load)
        {
            let t_now = TICK.load(Ordering::Relaxed);
            let sf = sched_fns().lock().unwrap_or_else(|e| e.into_inner());
            let n_sched = sf.len();
            write_varint(&mut *f, n_sched as i64);
            for (f2, due) in sf.iter() {
                (*f).extend_from_slice(&(f2.len() as u16).to_be_bytes());
                (*f).extend_from_slice(f2.as_bytes());
                (*f).extend_from_slice(&(due.saturating_sub(t_now)).to_be_bytes());
            }
            let ag = adv_done().lock().unwrap_or_else(|e| e.into_inner());
            write_varint(&mut *f, ag.len() as i64);
            let mut players: Vec<_> = ag.iter().collect();
            // Keep the original legacy layout when no IDs were earned, including
            // worlds with empty per-player entries; ADV1 is absent there too.
            if ag.values().any(|ids| !ids.is_empty()) {
                players.sort_unstable_by(|a, b| a.0.cmp(b.0));
            }
            for (pn, ids) in players {
                (*f).extend_from_slice(&(pn.len() as u16).to_be_bytes());
                (*f).extend_from_slice(pn.as_bytes());
                write_varint(&mut *f, ids.len() as i64);
                let mut ids: Vec<_> = ids.iter().collect();
                ids.sort_unstable();
                for id in ids {
                    (*f).extend_from_slice(&(id.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(id.as_bytes());
                }
            }
            // player attributes (p: keys) — e: keys are transient by design
            let at = attrs().lock().unwrap_or_else(|e| e.into_inner());
            let pa: Vec<(&String, &HashMap<String, f64>)> =
                at.iter().filter(|(k, _)| k.starts_with("p:")).collect();
            write_varint(&mut *f, pa.len() as i64);
            for (k, m) in pa {
                let nm = &k[2..];
                (*f).extend_from_slice(&(nm.len() as u16).to_be_bytes());
                (*f).extend_from_slice(nm.as_bytes());
                write_varint(&mut *f, m.len() as i64);
                for (an, v) in m.iter() {
                    (*f).extend_from_slice(&(an.len() as u16).to_be_bytes());
                    (*f).extend_from_slice(an.as_bytes());
                    (*f).extend_from_slice(&v.to_be_bytes());
                }
            }
            // Legacy objective criteria and sidebar selection.
            write_varint(&mut *f, scoreboard_objectives.len() as i64);
            for (obj, criterion) in scoreboard_objectives {
                let name = scoreboard_criterion_name(criterion);
                (*f).extend_from_slice(&(obj.len() as u16).to_be_bytes());
                (*f).extend_from_slice(obj.as_bytes());
                (*f).extend_from_slice(&(name.len() as u16).to_be_bytes());
                (*f).extend_from_slice(name.as_bytes());
            }
            (*f).extend_from_slice(&(scoreboard_sidebar.len() as u16).to_be_bytes());
            (*f).extend_from_slice(scoreboard_sidebar.as_bytes());
        }
    }

    // FAL2: falling-block entities (id, position, velocity, block state, age). FAL1 stored a
    // cell the old cell-moving model held; a save carrying it is converted on load.
    let mut in_flight = falling().lock().unwrap_or_else(|e| e.into_inner()).clone();
    in_flight.sort_by(|a, b| {
        (a.x.to_bits(), a.y.to_bits(), a.z.to_bits(), a.eid).cmp(&(
            b.x.to_bits(),
            b.y.to_bits(),
            b.z.to_bits(),
            b.eid,
        ))
    });
    (*f).extend_from_slice(b"FAL2");
    write_varint(&mut *f, in_flight.len() as i64);
    for fa in in_flight {
        (*f).extend_from_slice(&fa.eid.to_be_bytes());
        (*f).extend_from_slice(&fa.x.to_be_bytes());
        (*f).extend_from_slice(&fa.y.to_be_bytes());
        (*f).extend_from_slice(&fa.z.to_be_bytes());
        (*f).extend_from_slice(&fa.dy.to_be_bytes());
        (*f).extend_from_slice(&fa.state.to_be_bytes());
        (*f).extend_from_slice(&fa.time.to_be_bytes());
    }

    // RNG1: the world random stream's state. Without it a reload restarts the stream
    // from the seed, so two runs of the same seed diverge after a restart.
    (*f).extend_from_slice(b"RNG1");
    (*f).extend_from_slice(&world_rng::saved_state().to_be_bytes());
    let max_cramming = GR_MAX_ENTITY_CRAMMING.load(Ordering::Relaxed);
    if max_cramming != ENTITY_CRAMMING_DEFAULT {
        (*f).extend_from_slice(b"GCR1");
        (*f).extend_from_slice(&max_cramming.to_be_bytes());
    }
    // BRD1 stores the current interpolated extent and remaining transition time, so the
    // monotonic clock's origin is never persisted across process restarts.
    {
        let now_ms = border_now_ms();
        let border = *world_border().lock().unwrap_or_else(|e| e.into_inner());
        (*f).extend_from_slice(b"BRD1");
        (*f).extend_from_slice(&border.center_x.to_be_bytes());
        (*f).extend_from_slice(&border.center_z.to_be_bytes());
        (*f).extend_from_slice(&border.current_size(now_ms).to_be_bytes());
        (*f).extend_from_slice(&border.new_size.to_be_bytes());
        (*f).extend_from_slice(&border.lerp_remaining_ms(now_ms).max(0).to_be_bytes());
        (*f).extend_from_slice(&border.damage_per_block.to_be_bytes());
        (*f).extend_from_slice(&border.damage_safe_zone.to_be_bytes());
        (*f).extend_from_slice(&border.warning_blocks.to_be_bytes());
        (*f).extend_from_slice(&border.warning_time_secs.to_be_bytes());
        (*f).extend_from_slice(&border.absolute_max_size.to_be_bytes());
    }
    // Legacy world records have no orientation and default to zero.
    if world_spawn.3 != 0.0 {
        f.extend_from_slice(b"SPA1");
        f.extend_from_slice(&world_spawn.3.to_be_bytes());
    }
    append_block_entities_footer(f, &mut dxt);
    append_advancements_footer(f);
    append_save_state(f, &players, spawn_pitch, &ms, &mob_data);
    generated.append(f);
    let (sources, generated_blocks, generated_fluids) = worldgen::with_gen(|g| {
        (
            g.tick_sources(),
            g.generated_leaf_ticks().to_vec(),
            g.generated_fluid_ticks(),
        )
    })
    .unwrap_or_default();
    let blocks = block_ticks().lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (fluid_order, fluids, owned) = GENERATED_FLUID_OWNER
        .get()
        .map(|owner| {
            let owner = owner.lock().unwrap_or_else(|e| e.into_inner());
            (owner.next_order, owner.queue.clone(), owner.owned.clone())
        })
        .unwrap_or_else(|| (0, scheduled_ticks::ScheduledTicks::new(), HashSet::new()));
    tick_persistence::TickState {
        now: TICK_IDX.load(Ordering::Relaxed),
        block_order: BLOCK_TICK_ORDER.load(Ordering::Relaxed),
        blocks,
        fluid_order,
        fluids,
        owned,
        sources,
        generated_blocks,
        generated_fluids,
    }
    .append(f);
    if !drop_owners.is_empty() {
        append_drop_owners_footer(f, &drop_owners);
    }
    let mut block_loot: Vec<(BlockPos, PendingBlockLoot)> = Vec::new();
    for (&(dim, x, y, z), entry) in pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        if dim == DimId::Overworld {
            block_loot.push(((x, y, z), entry.clone()));
        } else if dim.is_vanilla() {
            dim2.loot.push(((dim, x, y, z), entry.clone()));
        } else {
            dxt.loot
                .push((dim_key(dim), x, y, z, entry.clone()));
        }
    }
    block_loot.sort_by_key(|(pos, _)| *pos);
    if !block_loot.is_empty() {
        append_block_loot_footer(f, &block_loot);
    }
    // Written last so it is read first: an Overworld-only save emits nothing here, leaving every
    // legacy byte (and every other footer's position) exactly where it was.
    if !dim_signs.is_empty() || !dim_players.is_empty() {
        append_dim_footer(f, &dim_signs, &dim_players);
    }
    // DIM2 is written after DIM1 and is therefore stripped before it on load. Emitted only when
    // something outside the Overworld actually exists.
    if !dim2.is_empty() {
        append_dim2_footer(f, &dim2);
    }
    append_brewing_footer(f);
    collect_dxt_brewing(&mut dxt);
    append_mob_dimensions_footer(f, &mob_dimensions);
    // Everything a custom dimension owns, named by its canonical key so a
    // datapack added, removed or reordered between restarts cannot move it.
    for spec in worldgen::custom_dimension_specs() {
        let mut table = Vec::new();
        if save_dim_overlay_into(spec.id, &mut table) > 0 {
            dxt.overlays.push((spec.key.to_string(), table));
        }
    }
    for (uuid, state) in players.iter() {
        let position = (!state.position.0.is_vanilla()).then(|| dim_key(state.position.0));
        let respawn = state
            .respawn
            .filter(|point| !point.dim.is_vanilla())
            .map(|point| dim_key(point.dim));
        if position.is_some() || respawn.is_some() {
            dxt.saved_players.push((*uuid, position, respawn));
        }
    }
    for (ordinal, dim) in mob_dimensions.iter().enumerate() {
        if !dim.is_vanilla() {
            dxt.mob_dims.push((ordinal as u32, dim_key(*dim)));
        }
    }
    for (ordinal, (dim, _)) in saved_raids().lock().unwrap_or_else(|e| e.into_inner()).iter().enumerate() {
        if !dim.is_vanilla() {
            dxt.raid_dims.push((ordinal as u32, dim_key(*dim)));
        }
    }
    append_dxt_footer(f, &dxt);
    // SGN1 tucks between DXT1 and WBE1: a world whose signs carry no dye,
    // glow or wax emits nothing here, keeping the exact pre-record tail.
    append_sign_meta_footer(f);
    // WBE1 tucks beneath WBD1: a registry with nothing in it emits not one
    // byte, so an untouched world keeps the exact pre-record tail.
    append_wbe_footer(f);
    // MOF1/AER1 tuck between WBE1 and WBD1: each is emitted only while its
    // mod bridge holds state, so an untouched vanilla save keeps the exact
    // pre-record tail byte-for-byte.
    append_mod_bridge_trailers(f);
    // WBD1 rides alongside VER1 beneath the seed trailer and, like every other
    // additive footer, is emitted only when it would say something a default
    // save does not already imply.
    if let Some(border) = save_world_border_record() {
        append_world_border_footer(f, &border);
    }
    // VER1 rides just beneath the seed trailer: `read_world_seed_footer` peels
    // both, so a reader that predates the record still sees its whole frame.
    // A native 1.21.1 save emits nothing, like the other default-only footers.
    if let Some(header) = save_version_header() {
        append_save_version_footer(f, &header);
    }
    append_world_seed_footer(f, WORLD_SEED.load(Ordering::Relaxed));
    // Keep full UUID records for players who go offline before the next save.
    *saved_players().lock().unwrap_or_else(|e| e.into_inner()) = players;
}

// ---------- save version header ----------
//
// Which Minecraft release a snapshot describes: its release name, the vanilla
// `DataVersion` integer and the network protocol version. Written as a VER1
// footer tucked just beneath the SED1 seed trailer — `read_world_seed_footer`
// peels both, so callers stripping the tail by hand (and every snapshot a
// pre-header build wrote) see the exact same bytes as before. Like the other
// additive footers, nothing is emitted for a native 1.21.1 save: the loader
// infers the same header from the record's absence.

const SAVE_VERSION_MAGIC: &[u8; 4] = b"VER1";

/// `(release name, vanilla DataVersion, network protocol)` for each milestone
/// the version table knows. The DataVersion column mirrors
/// `AnvilSchemaVersion`; the protocol column is the release's handshake number.
const SAVE_VERSION_TABLE: &[(&str, i32, i32)] = &[
    ("1.20.4", 3700, 765),
    ("1.20.6", 3839, 766),
    ("1.21", 3953, 767),
    ("1.21.1", 3955, 767),
    ("1.21.3", 4082, 768),
    ("1.21.4", 4189, 769),
    ("1.21.5", 4325, 770),
    ("26.3", 5023, 777),
];

/// The releases this build runs as: the two first-class targets of
/// `specs/multi-version-architecture.md` (`1.21.1` and `26.3`). The table
/// above can *describe* more milestones for diagnostics, but a running target
/// is only ever one of these — anything else fails closed to the 1.21.1
/// default.
const SAVE_TARGET_VERSIONS: &[&str] = &["1.21.1", "26.3"];

/// The save schema this build writes natively. The version header records it so
/// a future build can tell a save's vintage before parsing a byte of content.
const SAVE_DATA_VERSION_RUNNING: i32 = 3955;
const SAVE_PROTOCOL_VERSION_RUNNING: i32 = 767;

#[derive(Clone, Debug, PartialEq, Eq)]
struct SaveVersionHeader {
    mc_version: String,
    data_version: i32,
    protocol_version: i32,
    /// The file carried no VER1 record and the defaults were inferred rather
    /// than read. Diagnostic only — never on the wire.
    inferred_legacy_header: bool,
}

impl SaveVersionHeader {
    /// The header a snapshot written before VER1 existed gets: the only save
    /// schema this codebase had at the time.
    fn v1_21_1() -> Self {
        SaveVersionHeader {
            mc_version: "1.21.1".to_owned(),
            data_version: SAVE_DATA_VERSION_RUNNING,
            protocol_version: SAVE_PROTOCOL_VERSION_RUNNING,
            inferred_legacy_header: false,
        }
    }

    /// `v1_21_1()` marked as inferred, for a file with no VER1 record.
    fn legacy() -> Self {
        SaveVersionHeader {
            inferred_legacy_header: true,
            ..Self::v1_21_1()
        }
    }

    /// The header for a named release, or `None` when the version table does
    /// not know the name: nobody may guess a DataVersion pair for a release
    /// the table cannot describe.
    fn for_mc_version(mc_version: &str) -> Option<Self> {
        let name = mc_version.trim();
        SAVE_VERSION_TABLE
            .iter()
            .find(|(n, _, _)| *n == name)
            .map(|&(_, data_version, protocol_version)| SaveVersionHeader {
                mc_version: name.to_owned(),
                data_version,
                protocol_version,
                inferred_legacy_header: false,
            })
    }
}

/// The MC release this process writes and loads natively:
/// `MEGU_TARGET_VERSION`, then the legacy `MEGU_MC_VERSION`, each
/// expected to name one of the two first-class targets
/// (`SAVE_TARGET_VERSIONS`). An unset, empty or unrecognized variable is
/// skipped with a warning — the 1.21.1 default is the only silent choice.
fn running_save_version() -> SaveVersionHeader {
    for var in ["MEGU_TARGET_VERSION", "MEGU_MC_VERSION"] {
        let Ok(name) = std::env::var(var) else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        match SaveVersionHeader::for_mc_version(name) {
            Some(header) if SAVE_TARGET_VERSIONS.contains(&name) => return header,
            _ => eprintln!(
                "[save] {var}=`{name}` is not a supported target (1.21.1 or 26.3); ignoring"
            ),
        }
    }
    SaveVersionHeader::v1_21_1()
}

/// The record new snapshots carry: `Some` when the running target is not the
/// native 1.21.1 schema, else `None`. Like every other additive footer here, a
/// record that would only restate the native defaults is never emitted — a
/// 1.21.1 save keeps the exact pre-header byte layout and its loader infers
/// the same values instead.
fn save_version_header() -> Option<SaveVersionHeader> {
    let running = running_save_version();
    (running != SaveVersionHeader::v1_21_1()).then_some(running)
}

/// How a snapshot's DataVersion relates to this build's save schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveVersionCompat {
    /// Same DataVersion as the running build.
    Exact,
    /// The file predates the running schema; the migration lanes own the
    /// upgrade. `delta` is how many DataVersions behind the file is.
    OlderUpgradeable { delta: i32 },
    /// The file names a DataVersion newer than this build knows — reported so
    /// a downgrade can never pass for a native save and silently drop content.
    NewerUnknown { file_dv: i32, running_dv: i32 },
}

fn classify_save_v2_version(
    header: &SaveVersionHeader,
    running_data_version: i32,
) -> SaveVersionCompat {
    let file_dv = header.data_version;
    match file_dv.cmp(&running_data_version) {
        std::cmp::Ordering::Equal => SaveVersionCompat::Exact,
        std::cmp::Ordering::Less => SaveVersionCompat::OlderUpgradeable {
            delta: running_data_version - file_dv,
        },
        std::cmp::Ordering::Greater => SaveVersionCompat::NewerUnknown {
            file_dv,
            running_dv: running_data_version,
        },
    }
}

/// The version record of the most recently decoded snapshot, for diagnostics.
static LOADED_SAVE_VERSION: Mutex<Option<SaveVersionHeader>> = Mutex::new(None);

#[allow(dead_code)]
fn loaded_save_version_header() -> Option<SaveVersionHeader> {
    LOADED_SAVE_VERSION.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// `[schema u8][varint name len][name utf8][data_version i32][protocol i32]`,
/// framed `[body][body size u64][VER1]` like the other save-state footers.
fn append_save_version_footer(out: &mut Vec<u8>, header: &SaveVersionHeader) {
    let start = out.len();
    out.push(1); // VER1 schema version
    write_varint(out, header.mc_version.len() as i64);
    out.extend_from_slice(header.mc_version.as_bytes());
    save_numbers!(out, header.data_version, header.protocol_version);
    let size = (out.len() - start) as u64;
    save_numbers!(out, size);
    out.extend_from_slice(SAVE_VERSION_MAGIC);
}

/// Peel just the seed trailer, leaving any VER1 record in place for a caller
/// that reads it. `read_world_seed_footer` is the full strip every existing
/// caller expects.
fn strip_world_seed_footer(payload: &[u8]) -> Result<(&[u8], Option<u64>), &'static str> {
    if !payload.ends_with(WORLD_SEED_MAGIC) {
        return Ok((payload, None));
    }
    let end = payload
        .len()
        .checked_sub(12)
        .ok_or("truncated world seed footer")?;
    let seed = u64::from_be_bytes(
        payload[end..end + 8]
            .try_into()
            .map_err(|_| "truncated world seed footer")?,
    );
    Ok((&payload[..end], Some(seed)))
}

/// The VER1 record in `payload`, if present. The record sits just beneath the
/// seed trailer, so the trailer is lifted to reach it — this only reads, it
/// does not shorten `payload` for the caller: `read_world_seed_footer` is the
/// strip that hides VER1 exactly like a pre-header save does.
fn read_save_version_header(payload: &[u8]) -> Result<Option<SaveVersionHeader>, &'static str> {
    let (payload, _) = strip_world_seed_footer(payload)?;
    let (_, body) = split_save_footer(payload, SAVE_VERSION_MAGIC)?;
    let Some(body) = body else { return Ok(None) };
    let mut input = SaveInput(body);
    if input.byte()? != 1 {
        return Err("unsupported save version header");
    }
    let name_len =
        usize::try_from(input.varint()?).map_err(|_| "invalid save version name")?;
    let mc_version = std::str::from_utf8(input.bytes(name_len)?)
        .map_err(|_| "invalid save version name")?
        .to_owned();
    let data_version = i32::from_be_bytes(input.take()?);
    let protocol_version = i32::from_be_bytes(input.take()?);
    if !input.0.is_empty() {
        return Err("trailing bytes in save version header");
    }
    Ok(Some(SaveVersionHeader {
        mc_version,
        data_version,
        protocol_version,
        inferred_legacy_header: false,
    }))
}

// ---------- world border record ----------
//
// The static world-border settings a snapshot carries alongside VER1: the
// vanilla `level.dat` `Border*` fields minus the in-flight lerp, which BRD1
// keeps owning. Written as a WBD1 footer beneath SED1 — `read_world_seed_footer`
// peels all three records — and emitted only when the live border differs from
// the vanilla defaults, so a default save keeps the exact pre-record layout.

const WORLD_BORDER_MAGIC: &[u8; 4] = b"WBD1";

#[derive(Clone, Copy, Debug, PartialEq)]
struct WorldBorderSave {
    center_x: f64,
    center_z: f64,
    diameter: f64,
    damage_per_block: f64,
    damage_safe_zone: f64,
    warning_blocks: i32,
    warning_time: i32,
}

/// The vanilla `level.dat` defaults: a border at the origin reaching the
/// world edge, 0.2 damage per block past a 5-block safe zone, warning 5
/// blocks out and 15 seconds ahead.
const WORLD_BORDER_SAVE_DEFAULT: WorldBorderSave = WorldBorderSave {
    center_x: 0.0,
    center_z: 0.0,
    diameter: WORLD_BORDER_MAX_SIZE,
    damage_per_block: 0.2,
    damage_safe_zone: 5.0,
    warning_blocks: 5,
    warning_time: 15,
};

/// The record new snapshots carry, or `None` when the live border is already
/// the vanilla default: the `new_size` target is the diameter that survives a
/// load, while BRD1 keeps the live interpolation.
fn save_world_border_record() -> Option<WorldBorderSave> {
    let border = *world_border().lock().unwrap_or_else(|e| e.into_inner());
    let record = WorldBorderSave {
        center_x: border.center_x,
        center_z: border.center_z,
        diameter: border.new_size,
        damage_per_block: border.damage_per_block,
        damage_safe_zone: border.damage_safe_zone,
        warning_blocks: border.warning_blocks,
        warning_time: border.warning_time_secs,
    };
    (record != WORLD_BORDER_SAVE_DEFAULT).then_some(record)
}

/// `[schema u8][center_x f64][center_z f64][diameter f64][damage f64]
/// [safe zone f64][warning blocks i32][warning time i32]`, framed
/// `[body][body size u64][WBD1]` like the other save-state footers.
fn append_world_border_footer(out: &mut Vec<u8>, border: &WorldBorderSave) {
    let start = out.len();
    out.push(1); // WBD1 schema version
    save_numbers!(
        out,
        border.center_x,
        border.center_z,
        border.diameter,
        border.damage_per_block,
        border.damage_safe_zone,
        border.warning_blocks,
        border.warning_time
    );
    let size = (out.len() - start) as u64;
    save_numbers!(out, size);
    out.extend_from_slice(WORLD_BORDER_MAGIC);
}

/// The WBD1 record in `payload`, if present. The record sits just beneath the
/// VER1 record, which itself sits beneath the seed trailer; this only reads —
/// `read_world_seed_footer` is the strip that hides WBD1 exactly like a
/// pre-record save does.
fn read_world_border_footer(payload: &[u8]) -> Result<Option<WorldBorderSave>, &'static str> {
    let (payload, _) = strip_world_seed_footer(payload)?;
    let (payload, _) = split_save_footer(payload, SAVE_VERSION_MAGIC)?;
    let (_, body) = split_save_footer(payload, WORLD_BORDER_MAGIC)?;
    let Some(body) = body else { return Ok(None) };
    let mut input = SaveInput(body);
    if input.byte()? != 1 {
        return Err("unsupported world border footer");
    }
    let border = WorldBorderSave {
        center_x: input.f64()?,
        center_z: input.f64()?,
        diameter: input.f64()?,
        damage_per_block: input.f64()?,
        damage_safe_zone: input.f64()?,
        warning_blocks: i32::from_be_bytes(input.take()?),
        warning_time: i32::from_be_bytes(input.take()?),
    };
    if !input.0.is_empty() {
        return Err("trailing bytes in world border footer");
    }
    Ok(Some(border))
}

/// Install a WBD1 record as the live border: the diameter is the settled
/// target, so a saved lerp completes on load while BRD1 keeps any still-live
/// transition in saves that carry both records.
fn apply_saved_world_border(border: &WorldBorderSave) {
    *world_border().lock().unwrap_or_else(|e| e.into_inner()) = WorldBorderState {
        center_x: border.center_x,
        center_z: border.center_z,
        old_size: border.diameter,
        new_size: border.diameter,
        lerp_start_ms: border_now_ms(),
        lerp_duration_ms: 0,
        damage_per_block: border.damage_per_block,
        damage_safe_zone: border.damage_safe_zone,
        warning_blocks: border.warning_blocks,
        warning_time_secs: border.warning_time,
        absolute_max_size: WORLD_BORDER_MAX_CENTER,
    };
}

fn append_world_seed_footer(out: &mut Vec<u8>, seed: u64) {
    out.extend_from_slice(&seed.to_be_bytes());
    out.extend_from_slice(WORLD_SEED_MAGIC);
}

fn read_world_seed_footer(payload: &[u8]) -> Result<(&[u8], Option<u64>), &'static str> {
    let (payload, seed) = strip_world_seed_footer(payload)?;
    // The VER1 record rides just beneath the seed trailer and WBD1 beneath
    // VER1: peel both too, so callers that strip the tail by hand see the same
    // boundary a pre-record save had.
    let (payload, _) = split_save_footer(payload, SAVE_VERSION_MAGIC)?;
    let (payload, _) = split_save_footer(payload, WORLD_BORDER_MAGIC)?;
    Ok((payload, seed))
}

fn load_world_seed_in(dir: &std::path::Path) -> Result<Option<u64>, &'static str> {
    let path = dim_overlay_path(dir, DimId::Overworld);
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("failed to read world save"),
    };
    let payload = unwrap_world_payload(raw)?;
    let (_, seed) = read_world_seed_footer(&payload)?;
    Ok(seed)
}

fn resolve_world_seed(requested: Option<u64>, saved: Option<u64>) -> Result<u64, &'static str> {
    match (requested, saved) {
        (Some(requested), Some(saved)) if requested != saved => {
            Err("MEGU_SEED does not match the saved world seed")
        }
        (Some(requested), _) => Ok(requested),
        (None, Some(saved)) => Ok(saved),
        (None, None) => Ok(0),
    }
}

// One dimension byte per legacy mob record in the same ordinal order.
const MOB_DIM_MAGIC: &[u8; 4] = b"MDM1";

fn append_mob_dimensions_footer(out: &mut Vec<u8>, dimensions: &[DimId]) {
    if dimensions.iter().all(|dim| *dim == DimId::Overworld) {
        return;
    }
    out.extend(dimensions.iter().map(|dim| legacy_dim_byte(*dim)));
    out.extend_from_slice(&(dimensions.len() as u64).to_be_bytes());
    out.extend_from_slice(MOB_DIM_MAGIC);
}

fn read_mob_dimensions_footer(payload: &[u8]) -> Result<(&[u8], Vec<DimId>), &'static str> {
    if !payload.ends_with(MOB_DIM_MAGIC) {
        return Ok((payload, Vec::new()));
    }
    let end = payload
        .len()
        .checked_sub(12)
        .ok_or("truncated mob dimension footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid mob dimension footer size")?;
    if !(1..=MOB_CAP).contains(&size) {
        return Err("invalid mob dimension footer size");
    }
    let start = end
        .checked_sub(size)
        .ok_or("invalid mob dimension footer size")?;
    let dimensions = payload[start..end]
        .iter()
        .map(|byte| DimId::from_index(usize::from(*byte)).ok_or("invalid mob dimension"))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((&payload[..start], dimensions))
}

// Non-Overworld signs and player dimensions. The body is `[sign count][signs][player count][players]`,
// framed by `[body size u64][DIM1]` exactly like the drop-owners footer. A `DimId` byte is validated on
// read with `DimId::from_index`, so an unknown dimension fails closed instead of being silently
// mapped to the Overworld.
const DIM_FOOTER_LEN: usize = 12;
const DIM_MAGIC: &[u8; 4] = b"DIM1";
/// `(dim, x, y, z, facing, lines)` — one sign parked in a non-Overworld
/// dimension.
type DimSignRecord = (DimId, i32, i32, i32, u8, [String; 4]);
/// `(name, dim)` — the dimension a player logged out in.
type DimPlayerRecord = (String, DimId);
/// What `read_dim_footer` peels off: the remaining payload plus the two
/// record tables.
type DimFooterRecords<'a> = (&'a [u8], Vec<DimSignRecord>, Vec<DimPlayerRecord>);
fn append_dim_footer(
    out: &mut Vec<u8>,
    signs: &[DimSignRecord],
    players: &[DimPlayerRecord],
) {
    let start = out.len();
    out.extend_from_slice(&(signs.len() as u32).to_be_bytes());
    for &(dim, x, y, z, front, ref lines) in signs {
        out.push(dim.index() as u8);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        out.push(front);
        for l in lines.iter() {
            out.extend_from_slice(&(l.len() as u16).to_be_bytes());
            out.extend_from_slice(l.as_bytes());
        }
    }
    out.extend_from_slice(&(players.len() as u32).to_be_bytes());
    for (name, dim) in players {
        out.push(dim.index() as u8);
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
    }
    let size = (out.len() - start) as u64;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(DIM_MAGIC);
}
fn read_dim_footer(payload: &[u8]) -> Result<DimFooterRecords<'_>, &'static str> {
    if !payload.ends_with(DIM_MAGIC) {
        return Ok((payload, Vec::new(), Vec::new()));
    }
    let end = payload
        .len()
        .checked_sub(DIM_FOOTER_LEN)
        .ok_or("truncated dimension footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid dimension footer size")?;
    // Two u32 counts are the smallest possible body.
    if size < 8 {
        return Err("invalid dimension footer size");
    }
    let start = end.checked_sub(size).ok_or("invalid dimension footer size")?;
    let mut input = &payload[start..end];
    let read_dim = |input: &mut &[u8]| -> Result<DimId, &'static str> {
        let b = *input.first().ok_or("truncated dimension tag")?;
        *input = &input[1..];
        DimId::from_index(b as usize).ok_or("invalid dimension tag")
    };
    let n = u32::from_be_bytes(
        input
            .get(..4)
            .ok_or("truncated dimension sign count")?
            .try_into()
            .unwrap(),
    ) as usize;
    input = &input[4..];
    // Each sign has at least 1 dim + 13 position/front + 4 * 2 empty line lengths = 22 bytes.
    if n > input.len() / 22 {
        return Err("invalid dimension sign count");
    }
    let mut signs = Vec::with_capacity(n);
    for _ in 0..n {
        let dim = read_dim(&mut input)?;
        let pos = input.get(..13).ok_or("truncated dimension sign position")?;
        let x = i32::from_be_bytes(pos[..4].try_into().unwrap());
        let y = i32::from_be_bytes(pos[4..8].try_into().unwrap());
        let z = i32::from_be_bytes(pos[8..12].try_into().unwrap());
        let front = pos[12];
        input = &input[13..];
        let mut lines: [String; 4] = Default::default();
        for l in lines.iter_mut() {
            let nl = u16::from_be_bytes(
                input
                    .get(..2)
                    .ok_or("truncated dimension sign line length")?
                    .try_into()
                    .unwrap(),
            ) as usize;
            input = &input[2..];
            if input.len() < nl {
                return Err("truncated dimension sign line");
            }
            *l = String::from_utf8_lossy(&input[..nl]).to_string();
            input = &input[nl..];
        }
        signs.push((dim, x, y, z, front, lines));
    }
    let pn = u32::from_be_bytes(
        input
            .get(..4)
            .ok_or("truncated dimension player count")?
            .try_into()
            .unwrap(),
    ) as usize;
    input = &input[4..];
    // Each player has at least 1 dim + 2 name length + 0 name bytes = 3 bytes.
    if pn > input.len() / 3 {
        return Err("invalid dimension player count");
    }
    let mut players = Vec::with_capacity(pn);
    for _ in 0..pn {
        let dim = read_dim(&mut input)?;
        let nl = u16::from_be_bytes(
            input
                .get(..2)
                .ok_or("truncated dimension player name length")?
                .try_into()
                .unwrap(),
        ) as usize;
        input = &input[2..];
        if input.len() < nl {
            return Err("truncated dimension player name");
        }
        let name = String::from_utf8_lossy(&input[..nl]).to_string();
        input = &input[nl..];
        players.push((name, dim));
    }
    if !input.is_empty() {
        return Err("invalid dimension footer tail");
    }
    Ok((&payload[..start], signs, players))
}

/// Non-Overworld block containers, furnaces, crops (and their pending loot) ride in this footer.
/// The legacy sections keep their exact wire layout and hold the Overworld's entries only, so an
/// Overworld-only save emits nothing here and stays byte-for-byte identical to the old file.
const DIM2_FOOTER_LEN: usize = 12;
const DIM2_MAGIC: &[u8; 4] = b"DIM2";
#[derive(Default)]
struct Dim2Entries {
    chests: Vec<(DimBlockPos, HashMap<i16, Stack>)>,
    furnaces: Vec<(DimBlockPos, Furnace)>,
    crops: Vec<(DimBlockPos, u8)>,
    loot: Vec<(DimBlockPos, PendingBlockLoot)>,
}
impl Dim2Entries {
    fn is_empty(&self) -> bool {
        self.chests.is_empty()
            && self.furnaces.is_empty()
            && self.crops.is_empty()
            && self.loot.is_empty()
    }
}
fn append_dim2_footer(out: &mut Vec<u8>, entries: &Dim2Entries) {
    use crate::generation_payload::ContainerKind;
    let start = out.len();
    out.extend_from_slice(&(entries.chests.len() as u32).to_be_bytes());
    for ((dim, x, y, z), slots) in &entries.chests {
        out.push(dim.index() as u8);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        write_stack_table(out, slots);
    }
    out.extend_from_slice(&(entries.furnaces.len() as u32).to_be_bytes());
    for ((dim, x, y, z), fr) in &entries.furnaces {
        out.push(dim.index() as u8);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        write_furnace_state(out, fr);
    }
    out.extend_from_slice(&(entries.crops.len() as u32).to_be_bytes());
    for ((dim, x, y, z), age) in &entries.crops {
        out.push(dim.index() as u8);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        out.push(*age);
    }
    out.extend_from_slice(&(entries.loot.len() as u32).to_be_bytes());
    for ((dim, x, y, z), entry) in &entries.loot {
        out.push(dim.index() as u8);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        match entry.kind {
            ContainerKind::Chest => out.push(0),
            ContainerKind::Furnace => out.push(1),
            ContainerKind::BlockContainer(size) => {
                out.push(2);
                out.push(size);
            }
        }
        let table = entry.loot_table.as_bytes();
        let tlen = table.len().min(u16::MAX as usize);
        out.extend_from_slice(&(tlen as u16).to_be_bytes());
        out.extend_from_slice(&table[..tlen]);
        if let Some(seed) = entry.loot_seed {
            out.push(1);
            out.extend_from_slice(&seed.to_be_bytes());
        } else {
            out.push(0);
        }
    }
    let size = (out.len() - start) as u64;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(DIM2_MAGIC);
}
/// `DimId::from_index` is the fail-closed gate: an unknown tag, or an Overworld tag in a footer
/// that is only supposed to carry the other dimensions, is an error rather than a silent
/// relocation into the Overworld.
fn read_dim2_dim(body: &[u8], p: &mut usize) -> Result<DimId, &'static str> {
    let b = *body.get(*p).ok_or("truncated DIM2 dimension tag")?;
    *p += 1;
    let dim = DimId::from_index(b as usize).ok_or("invalid DIM2 dimension tag")?;
    if dim == DimId::Overworld {
        return Err("Overworld entry in DIM2 footer");
    }
    Ok(dim)
}
fn read_dim2_pos(body: &[u8], p: &mut usize) -> Result<DimBlockPos, &'static str> {
    let dim = read_dim2_dim(body, p)?;
    let raw = body
        .get(*p..*p + 12)
        .ok_or("truncated DIM2 position")?;
    *p += 12;
    Ok((
        dim,
        i32::from_be_bytes(raw[0..4].try_into().unwrap()),
        i32::from_be_bytes(raw[4..8].try_into().unwrap()),
        i32::from_be_bytes(raw[8..12].try_into().unwrap()),
    ))
}
fn read_dim2_count(body: &[u8], p: &mut usize, min_entry: usize) -> Result<usize, &'static str> {
    let n = u32::from_be_bytes(
        body.get(*p..*p + 4)
            .ok_or("truncated DIM2 count")?
            .try_into()
            .unwrap(),
    ) as usize;
    *p += 4;
    if n > body.len().saturating_sub(*p) / min_entry {
        return Err("invalid DIM2 count");
    }
    Ok(n)
}
fn read_dim2_footer(payload: &[u8]) -> Result<(&[u8], Dim2Entries), &'static str> {
    if !payload.ends_with(DIM2_MAGIC) {
        return Ok((payload, Dim2Entries::default()));
    }
    // Four u32 counts make the smallest possible body.
    let end = payload
        .len()
        .checked_sub(DIM2_FOOTER_LEN)
        .ok_or("truncated DIM2 footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid DIM2 footer size")?;
    if size < 16 {
        return Err("invalid DIM2 footer size");
    }
    let start = end.checked_sub(size).ok_or("invalid DIM2 footer size")?;
    let body = &payload[start..end];
    let mut p = 0usize;
    let mut out = Dim2Entries::default();
    let n = read_dim2_count(body, &mut p, 14)?;
    for _ in 0..n {
        let pos = read_dim2_pos(body, &mut p)?;
        let slots = read_stack_table(body, &mut p, true);
        out.chests.push((pos, slots));
    }
    let n = read_dim2_count(body, &mut p, 14)?;
    for _ in 0..n {
        let pos = read_dim2_pos(body, &mut p)?;
        let fr = read_furnace_state(body, &mut p, true, false)
            .ok_or("truncated DIM2 furnace")?;
        out.furnaces.push((pos, fr));
    }
    let n = read_dim2_count(body, &mut p, 14)?;
    for _ in 0..n {
        let pos = read_dim2_pos(body, &mut p)?;
        let age = *body.get(p).ok_or("truncated DIM2 crop age")?;
        p += 1;
        out.crops.push((pos, age));
    }
    {
        use crate::generation_payload::ContainerKind;
        let n = read_dim2_count(body, &mut p, 17)?;
        for _ in 0..n {
            let pos = read_dim2_pos(body, &mut p)?;
            let tag = *body.get(p).ok_or("truncated DIM2 loot kind")?;
            p += 1;
            let kind = match tag {
                0 => ContainerKind::Chest,
                1 => ContainerKind::Furnace,
                2 => {
                    let sz = *body.get(p).ok_or("truncated DIM2 loot size")?;
                    p += 1;
                    ContainerKind::BlockContainer(sz)
                }
                _ => return Err("invalid DIM2 loot kind"),
            };
            let tlen = u16::from_be_bytes(
                body.get(p..p + 2)
                    .ok_or("truncated DIM2 loot table length")?
                    .try_into()
                    .unwrap(),
            ) as usize;
            p += 2;
            let table = body.get(p..p + tlen).ok_or("truncated DIM2 loot table")?;
            p += tlen;
            let loot_table = String::from_utf8_lossy(table).to_string();
            let has_seed = *body.get(p).ok_or("truncated DIM2 loot seed flag")?;
            p += 1;
            let loot_seed = match has_seed {
                0 => None,
                1 => {
                    let s = i64::from_be_bytes(
                        body.get(p..p + 8)
                            .ok_or("truncated DIM2 loot seed")?
                            .try_into()
                            .unwrap(),
                    );
                    p += 8;
                    Some(s)
                }
                _ => return Err("invalid DIM2 loot seed flag"),
            };
            out.loot.push((
                pos,
                PendingBlockLoot {
                    kind,
                    loot_table,
                    loot_seed,
                },
            ));
        }
    }
    if p != body.len() {
        return Err("invalid DIM2 footer tail");
    }
    Ok((&payload[..start], out))
}

// ---------- custom-dimension extension (DXT1) ----------
//
// Custom dimensions have no stable slot: a datapack added, removed or reordered
// between restarts moves every index at or above the first custom slot, so the
// one-byte `DimId` encoding the other footers use cannot name one. Every record a
// custom dimension owns rides here instead, keyed by its canonical
// `"namespace:path"` string. The footer is written last, so a save without a
// custom dimension keeps every byte of the legacy layout — and every earlier
// footer's position — exactly where it was.
//
// Version 1 body layout:
//   u8 version(=1)
//   varint overlay count, then per dimension `u16 key`, `u32 table_len`, the
//     overlay table `save_dim_overlay_into` writes verbatim
//   varint pos_store count, then `u16 name` + `u16 key` player-dimension fixes
//   varint saved-player count, then `uuid` + `u8 flags` (bit0 position,
//     bit1 respawn) + `u16 key` per set flag
//   varint mob count, then `varint ordinal` + `u16 key`
//   varint raid count, then `varint ordinal` + `u16 key`
//   u32 sign count, then `u16 key` + xyz + front + four `u16` lines
//   u32 chest count, then `u16 key` + xyz + stack table
//   u32 furnace count, then `u16 key` + xyz + furnace state
//   u32 crop count, then `u16 key` + xyz + `u8` age
//   u32 loot count, then `u16 key` + xyz + loot record
//   u32 brewing count, then `u16 key` + xyz + stack table + fuel + `i32` timer
//   u32 block-entity count, then `u16 key` + xyz + `u32` state + `u32`-framed NBT
//   u64 body size, "DXT1"
const DXT_MAGIC: &[u8; 4] = b"DXT1";

/// `(key, x, y, z, slots)` — a parked chest-like container outside the
/// Overworld.
type DxtChestRecord = (String, i32, i32, i32, HashMap<i16, Stack>);
/// `(key, x, y, z, slots, fuel, brew time)` — a parked brewing stand.
type DxtBrewingRecord = (String, i32, i32, i32, HashMap<i16, Stack>, u8, u16);

#[derive(Default)]
struct DxtEntries {
    overlays: Vec<(String, Vec<u8>)>,
    players: Vec<(String, String)>,
    saved_players: Vec<([u8; 16], Option<String>, Option<String>)>,
    mob_dims: Vec<(u32, String)>,
    raid_dims: Vec<(u32, String)>,
    signs: Vec<(String, i32, i32, i32, u8, [String; 4])>,
    chests: Vec<DxtChestRecord>,
    furnaces: Vec<(String, i32, i32, i32, Furnace)>,
    crops: Vec<(String, i32, i32, i32, u8)>,
    loot: Vec<(String, i32, i32, i32, PendingBlockLoot)>,
    brewing: Vec<DxtBrewingRecord>,
    block_entities: Vec<(String, i32, i32, i32, u32, nbt::Tag)>,
}

impl DxtEntries {
    fn is_empty(&self) -> bool {
        self.overlays.is_empty()
            && self.players.is_empty()
            && self.saved_players.is_empty()
            && self.mob_dims.is_empty()
            && self.raid_dims.is_empty()
            && self.signs.is_empty()
            && self.chests.is_empty()
            && self.furnaces.is_empty()
            && self.crops.is_empty()
            && self.loot.is_empty()
            && self.brewing.is_empty()
            && self.block_entities.is_empty()
    }
}

/// The one-byte save encoding for a dimension whose records live in a legacy
/// footer. A custom dimension's records never reach those footers — they ride
/// DXT1 by name — so the byte is a placeholder the reader never sees; `0` keeps
/// it deterministic.
fn legacy_dim_byte(dim: DimId) -> u8 {
    if dim.is_vanilla() {
        dim.index() as u8
    } else {
        0
    }
}

/// A dimension's canonical `"namespace:path"` — the identity DXT1 records carry.
fn dim_key(dim: DimId) -> String {
    dim.dimension_id().to_string()
}

/// Resolve a DXT1 dimension name. A datapack dropped between restarts leaves the
/// key unresolvable; the record is skipped with a diagnostic rather than silently
/// relocated into the Overworld.
fn dxt_dim(key: &str) -> Option<DimId> {
    let dim = DimId::parse_dimension_id(key);
    if dim.is_none() {
        eprintln!("[spine] save references unregistered dimension {key}: skipping its records");
    }
    dim
}

fn write_dxt_key(out: &mut Vec<u8>, key: &str) {
    out.extend_from_slice(&(key.len() as u16).to_be_bytes());
    out.extend_from_slice(key.as_bytes());
}

fn read_dxt_key<'a>(body: &'a [u8], p: &mut usize) -> Result<&'a str, &'static str> {
    let len = u16::from_be_bytes(
        body.get(*p..*p + 2)
            .ok_or("truncated DXT key length")?
            .try_into()
            .unwrap(),
    ) as usize;
    *p += 2;
    let key = body.get(*p..*p + len).ok_or("truncated DXT key")?;
    *p += len;
    std::str::from_utf8(key).map_err(|_| "invalid DXT key")
}

fn write_dxt_pos(out: &mut Vec<u8>, key: &str, x: i32, y: i32, z: i32) {
    write_dxt_key(out, key);
    out.extend_from_slice(&x.to_be_bytes());
    out.extend_from_slice(&y.to_be_bytes());
    out.extend_from_slice(&z.to_be_bytes());
}

fn read_dxt_pos<'a>(
    body: &'a [u8],
    p: &mut usize,
) -> Result<(&'a str, i32, i32, i32), &'static str> {
    let key = read_dxt_key(body, p)?;
    let raw = body.get(*p..*p + 12).ok_or("truncated DXT position")?;
    *p += 12;
    Ok((
        key,
        i32::from_be_bytes(raw[0..4].try_into().unwrap()),
        i32::from_be_bytes(raw[4..8].try_into().unwrap()),
        i32::from_be_bytes(raw[8..12].try_into().unwrap()),
    ))
}

fn append_dxt_footer(out: &mut Vec<u8>, entries: &DxtEntries) {
    use crate::generation_payload::ContainerKind;
    // A save with no custom-dimension state emits no footer at all — vanilla
    // worlds keep the exact legacy byte layout.
    if entries.is_empty() {
        return;
    }
    let start = out.len();
    out.push(1); // version
    write_varint(out, entries.overlays.len() as i64);
    for (key, table) in &entries.overlays {
        write_dxt_key(out, key);
        out.extend_from_slice(&(table.len() as u32).to_be_bytes());
        out.extend_from_slice(table);
    }
    write_varint(out, entries.players.len() as i64);
    for (name, key) in &entries.players {
        out.extend_from_slice(&(name.len() as u16).to_be_bytes());
        out.extend_from_slice(name.as_bytes());
        write_dxt_key(out, key);
    }
    write_varint(out, entries.saved_players.len() as i64);
    for (uuid, position, respawn) in &entries.saved_players {
        out.extend_from_slice(uuid);
        out.push(
            u8::from(position.is_some()) | (u8::from(respawn.is_some()) << 1),
        );
        for key in [position, respawn].into_iter().flatten() {
            write_dxt_key(out, key);
        }
    }
    write_varint(out, entries.mob_dims.len() as i64);
    for (ordinal, key) in &entries.mob_dims {
        write_varint(out, i64::from(*ordinal));
        write_dxt_key(out, key);
    }
    write_varint(out, entries.raid_dims.len() as i64);
    for (ordinal, key) in &entries.raid_dims {
        write_varint(out, i64::from(*ordinal));
        write_dxt_key(out, key);
    }
    out.extend_from_slice(&(entries.signs.len() as u32).to_be_bytes());
    for (key, x, y, z, front, lines) in &entries.signs {
        write_dxt_pos(out, key, *x, *y, *z);
        out.push(*front);
        for line in lines {
            out.extend_from_slice(&(line.len() as u16).to_be_bytes());
            out.extend_from_slice(line.as_bytes());
        }
    }
    out.extend_from_slice(&(entries.chests.len() as u32).to_be_bytes());
    for (key, x, y, z, slots) in &entries.chests {
        write_dxt_pos(out, key, *x, *y, *z);
        write_stack_table(out, slots);
    }
    out.extend_from_slice(&(entries.furnaces.len() as u32).to_be_bytes());
    for (key, x, y, z, fr) in &entries.furnaces {
        write_dxt_pos(out, key, *x, *y, *z);
        write_furnace_state(out, fr);
    }
    out.extend_from_slice(&(entries.crops.len() as u32).to_be_bytes());
    for (key, x, y, z, age) in &entries.crops {
        write_dxt_pos(out, key, *x, *y, *z);
        out.push(*age);
    }
    out.extend_from_slice(&(entries.loot.len() as u32).to_be_bytes());
    for (key, x, y, z, entry) in &entries.loot {
        write_dxt_pos(out, key, *x, *y, *z);
        match entry.kind {
            ContainerKind::Chest => out.push(0),
            ContainerKind::Furnace => out.push(1),
            ContainerKind::BlockContainer(size) => {
                out.push(2);
                out.push(size);
            }
        }
        let table = entry.loot_table.as_bytes();
        let tlen = table.len().min(u16::MAX as usize);
        out.extend_from_slice(&(tlen as u16).to_be_bytes());
        out.extend_from_slice(&table[..tlen]);
        if let Some(seed) = entry.loot_seed {
            out.push(1);
            out.extend_from_slice(&seed.to_be_bytes());
        } else {
            out.push(0);
        }
    }
    out.extend_from_slice(&(entries.brewing.len() as u32).to_be_bytes());
    for (key, x, y, z, slots, fuel, remaining) in &entries.brewing {
        write_dxt_pos(out, key, *x, *y, *z);
        write_stack_table(out, slots);
        out.push(*fuel);
        out.extend_from_slice(&remaining.to_be_bytes());
    }
    out.extend_from_slice(&(entries.block_entities.len() as u32).to_be_bytes());
    for (key, x, y, z, state, tag) in &entries.block_entities {
        write_dxt_pos(out, key, *x, *y, *z);
        out.extend_from_slice(&state.to_be_bytes());
        let encoded =
            nbt::write_named(&nbt::NbtString::text(""), tag).expect("block entity NBT must encode");
        out.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        out.extend_from_slice(&encoded);
    }
    let size = (out.len() - start) as u64;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(DXT_MAGIC);
}

fn read_dxt_footer(payload: &[u8]) -> Result<(&[u8], DxtEntries), &'static str> {
    if !payload.ends_with(DXT_MAGIC) {
        return Ok((payload, DxtEntries::default()));
    }
    let end = payload
        .len()
        .checked_sub(12)
        .ok_or("truncated DXT footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid DXT footer size")?;
    let start = end.checked_sub(size).ok_or("invalid DXT footer size")?;
    let body = &payload[start..end];
    let mut p = 0usize;
    let mut out = DxtEntries::default();
    let version = *body.get(p).ok_or("truncated DXT footer")?;
    p += 1;
    if version != 1 {
        return Err("unsupported DXT version");
    }
    let mut input = SaveInput(&body[p..]);
    let overlay_count = input.var_count(10)?;
    p = body.len() - input.0.len();
    for _ in 0..overlay_count {
        let key = read_dxt_key(body, &mut p)?.to_string();
        let tlen = u32::from_be_bytes(
            body.get(p..p + 4)
                .ok_or("truncated DXT overlay table length")?
                .try_into()
                .unwrap(),
        ) as usize;
        p += 4;
        let table = body
            .get(p..p + tlen)
            .ok_or("truncated DXT overlay table")?
            .to_vec();
        p += tlen;
        out.overlays.push((key, table));
    }
    let mut input = SaveInput(&body[p..]);
    let player_count = input.var_count(10)?;
    p = body.len() - input.0.len();
    for _ in 0..player_count {
        let nlen = u16::from_be_bytes(
            body.get(p..p + 2)
                .ok_or("truncated DXT player name")?
                .try_into()
                .unwrap(),
        ) as usize;
        p += 2;
        let name = body
            .get(p..p + nlen)
            .ok_or("truncated DXT player name")?;
        p += nlen;
        let name = std::str::from_utf8(name)
            .map_err(|_| "invalid DXT player name")?
            .to_string();
        let key = read_dxt_key(body, &mut p)?.to_string();
        out.players.push((name, key));
    }
    let mut input = SaveInput(&body[p..]);
    let saved_count = input.var_count(10)?;
    p = body.len() - input.0.len();
    for _ in 0..saved_count {
        let uuid: [u8; 16] = body
            .get(p..p + 16)
            .ok_or("truncated DXT saved player")?
            .try_into()
            .unwrap();
        p += 16;
        let flags = *body.get(p).ok_or("truncated DXT saved player")?;
        p += 1;
        let mut fields = [None, None];
        for (bit, field) in fields.iter_mut().enumerate() {
            if flags & (1 << bit) != 0 {
                *field = Some(read_dxt_key(body, &mut p)?.to_string());
            }
        }
        let [position, respawn] = fields;
        out.saved_players.push((uuid, position, respawn));
    }
    for dims in [&mut out.mob_dims, &mut out.raid_dims] {
        let mut input = SaveInput(&body[p..]);
        let count = input.var_count(10)?;
        p = body.len() - input.0.len();
        for _ in 0..count {
            let mut input = SaveInput(&body[p..]);
            let ordinal = u32::try_from(input.varint()?).map_err(|_| "invalid DXT ordinal")?;
            p = body.len() - input.0.len();
            let key = read_dxt_key(body, &mut p)?.to_string();
            dims.push((ordinal, key));
        }
    }
    let n = read_dim2_count(body, &mut p, 15)?;
    for _ in 0..n {
        let (key, x, y, z) = read_dxt_pos(body, &mut p)?;
        let front = *body.get(p).ok_or("truncated DXT sign")?;
        p += 1;
        let mut lines: [String; 4] = Default::default();
        for line in lines.iter_mut() {
            let llen = u16::from_be_bytes(
                body.get(p..p + 2)
                    .ok_or("truncated DXT sign line")?
                    .try_into()
                    .unwrap(),
            ) as usize;
            p += 2;
            let text = body.get(p..p + llen).ok_or("truncated DXT sign line")?;
            p += llen;
            *line = String::from_utf8_lossy(text).to_string();
        }
        out.signs.push((key.to_string(), x, y, z, front, lines));
    }
    let n = read_dim2_count(body, &mut p, 15)?;
    for _ in 0..n {
        let (key, x, y, z) = read_dxt_pos(body, &mut p)?;
        let slots = read_stack_table(body, &mut p, true);
        out.chests.push((key.to_string(), x, y, z, slots));
    }
    let n = read_dim2_count(body, &mut p, 15)?;
    for _ in 0..n {
        let (key, x, y, z) = read_dxt_pos(body, &mut p)?;
        let fr = read_furnace_state(body, &mut p, true, false)
            .ok_or("truncated DXT furnace")?;
        out.furnaces.push((key.to_string(), x, y, z, fr));
    }
    let n = read_dim2_count(body, &mut p, 16)?;
    for _ in 0..n {
        let (key, x, y, z) = read_dxt_pos(body, &mut p)?;
        let age = *body.get(p).ok_or("truncated DXT crop age")?;
        p += 1;
        out.crops.push((key.to_string(), x, y, z, age));
    }
    {
        use crate::generation_payload::ContainerKind;
        let n = read_dim2_count(body, &mut p, 18)?;
        for _ in 0..n {
            let (key, x, y, z) = read_dxt_pos(body, &mut p)?;
            let tag = *body.get(p).ok_or("truncated DXT loot kind")?;
            p += 1;
            let kind = match tag {
                0 => ContainerKind::Chest,
                1 => ContainerKind::Furnace,
                2 => {
                    let sz = *body.get(p).ok_or("truncated DXT loot size")?;
                    p += 1;
                    ContainerKind::BlockContainer(sz)
                }
                _ => return Err("invalid DXT loot kind"),
            };
            let tlen = u16::from_be_bytes(
                body.get(p..p + 2)
                    .ok_or("truncated DXT loot table length")?
                    .try_into()
                    .unwrap(),
            ) as usize;
            p += 2;
            let table = body.get(p..p + tlen).ok_or("truncated DXT loot table")?;
            p += tlen;
            let loot_table = String::from_utf8_lossy(table).to_string();
            let has_seed = *body.get(p).ok_or("truncated DXT loot seed flag")?;
            p += 1;
            let loot_seed = match has_seed {
                0 => None,
                1 => {
                    let s = i64::from_be_bytes(
                        body.get(p..p + 8)
                            .ok_or("truncated DXT loot seed")?
                            .try_into()
                            .unwrap(),
                    );
                    p += 8;
                    Some(s)
                }
                _ => return Err("invalid DXT loot seed flag"),
            };
            out.loot.push((
                key.to_string(),
                x,
                y,
                z,
                PendingBlockLoot {
                    kind,
                    loot_table,
                    loot_seed,
                },
            ));
        }
    }
    let n = read_dim2_count(body, &mut p, 15)?;
    for _ in 0..n {
        let (key, x, y, z) = read_dxt_pos(body, &mut p)?;
        let slots = read_stack_table(body, &mut p, true);
        let timers = body.get(p..p + 5).ok_or("truncated DXT brewing stand")?;
        p += timers.len();
        let fuel = timers[0];
        let brew_time = u32::from_be_bytes(timers[1..5].try_into().unwrap());
        if fuel > BREW_FUEL_PER_POWDER || brew_time > u32::from(BREW_TIME_TICKS) {
            return Err("invalid DXT brewing stand timers");
        }
        out.brewing.push((
            key.to_string(),
            x,
            y,
            z,
            slots,
            fuel,
            brew_time as u16,
        ));
    }
    let n = read_dim2_count(body, &mut p, 23)?;
    for _ in 0..n {
        let (key, x, y, z) = read_dxt_pos(body, &mut p)?;
        let state = u32::from_be_bytes(
            body.get(p..p + 4)
                .ok_or("truncated DXT block entity")?
                .try_into()
                .unwrap(),
        );
        p += 4;
        let len = u32::from_be_bytes(
            body.get(p..p + 4)
                .ok_or("truncated DXT block entity NBT")?
                .try_into()
                .unwrap(),
        ) as usize;
        p += 4;
        let data = body.get(p..p + len).ok_or("truncated DXT block entity NBT")?;
        p += len;
        let (_, tag) = nbt::read_named(data).map_err(|_| "invalid DXT block entity NBT")?;
        out.block_entities
            .push((key.to_string(), x, y, z, state, tag));
    }
    if p != body.len() {
        return Err("invalid DXT footer tail");
    }
    Ok((&payload[..start], out))
}

const BLOCK_LOOT_FOOTER_LEN: usize = 12;
const BLOCK_LOOT_MAGIC: &[u8; 4] = b"BLT1";
fn append_block_loot_footer(out: &mut Vec<u8>, entries: &[(BlockPos, PendingBlockLoot)]) {
    use crate::generation_payload::ContainerKind;
    let start = out.len();
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for &(pos, ref entry) in entries {
        out.extend_from_slice(&pos.0.to_be_bytes());
        out.extend_from_slice(&pos.1.to_be_bytes());
        out.extend_from_slice(&pos.2.to_be_bytes());
        match entry.kind {
            ContainerKind::Chest => out.push(0),
            ContainerKind::Furnace => out.push(1),
            ContainerKind::BlockContainer(size) => {
                out.push(2);
                out.push(size);
            }
        }
        let table = entry.loot_table.as_bytes();
        let tlen = table.len().min(u16::MAX as usize);
        out.extend_from_slice(&(tlen as u16).to_be_bytes());
        out.extend_from_slice(&table[..tlen]);
        if let Some(seed) = entry.loot_seed {
            out.push(1);
            out.extend_from_slice(&seed.to_be_bytes());
        } else {
            out.push(0);
        }
    }
    let size = (out.len() - start) as u64;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(BLOCK_LOOT_MAGIC);
}
fn read_block_loot_footer(
    payload: &[u8],
) -> Result<(&[u8], HashMap<BlockPos, PendingBlockLoot>), &'static str> {
    use crate::generation_payload::ContainerKind;
    if !payload.ends_with(BLOCK_LOOT_MAGIC) {
        return Ok((payload, HashMap::new()));
    }
    let end = payload
        .len()
        .checked_sub(BLOCK_LOOT_FOOTER_LEN)
        .ok_or("truncated block loot footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid block loot footer size")?;
    if size == 0 {
        return Err("invalid block loot footer size");
    }
    let start = end
        .checked_sub(size)
        .ok_or("invalid block loot footer size")?;
    let mut input = &payload[start..end];
    if input.len() < 4 {
        return Err("truncated block loot count");
    }
    let count = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
    input = &input[4..];
    if count > input.len() / 16 {
        return Err("invalid block loot count");
    }
    let mut out = HashMap::with_capacity(count);
    for _ in 0..count {
        if input.len() < 13 {
            return Err("truncated block loot entry");
        }
        let x = i32::from_be_bytes(input[..4].try_into().unwrap());
        let y = i32::from_be_bytes(input[4..8].try_into().unwrap());
        let z = i32::from_be_bytes(input[8..12].try_into().unwrap());
        let tag = input[12];
        input = &input[13..];
        let kind = match tag {
            0 => ContainerKind::Chest,
            1 => ContainerKind::Furnace,
            2 => {
                if input.is_empty() {
                    return Err("truncated block loot size");
                }
                let sz = input[0];
                input = &input[1..];
                ContainerKind::BlockContainer(sz)
            }
            _ => return Err("invalid block loot kind"),
        };
        if input.len() < 2 {
            return Err("truncated block loot table length");
        }
        let tlen = u16::from_be_bytes(input[..2].try_into().unwrap()) as usize;
        input = &input[2..];
        if input.len() < tlen + 1 {
            return Err("truncated block loot table");
        }
        let loot_table = String::from_utf8_lossy(&input[..tlen]).to_string();
        input = &input[tlen..];
        let has_seed = input[0];
        input = &input[1..];
        let loot_seed = match has_seed {
            0 => None,
            1 => {
                if input.len() < 8 {
                    return Err("truncated block loot seed");
                }
                let s = i64::from_be_bytes(input[..8].try_into().unwrap());
                input = &input[8..];
                Some(s)
            }
            _ => return Err("invalid block loot seed flag"),
        };
        out.insert(
            (x, y, z),
            PendingBlockLoot {
                kind,
                loot_table,
                loot_seed,
            },
        );
    }
    if !input.is_empty() {
        return Err("invalid block loot tail");
    }
    Ok((&payload[..start], out))
}
const DROP_OWNERS_FOOTER_LEN: usize = 12;
const DROP_OWNERS_MAGIC: &[u8; 4] = b"DRO1";
/// `(index, target owner, thrower)` as written: the `u32` is the drop's
/// position in the entity record order.
type DropOwnerRecord = (u32, Option<[u8; 16]>, Option<[u8; 16]>);
/// The same record with the index widened to `usize` on the read path.
type DropOwnerIndex = (usize, Option<[u8; 16]>, Option<[u8; 16]>);
fn append_drop_owners_footer(out: &mut Vec<u8>, owners: &[DropOwnerRecord]) {
    let start = out.len();
    out.extend_from_slice(&(owners.len() as u32).to_be_bytes());
    for &(idx, target, thrower) in owners {
        out.extend_from_slice(&idx.to_be_bytes());
        let flags = u8::from(target.is_some()) | (u8::from(thrower.is_some()) << 1);
        out.push(flags);
        if let Some(t) = target {
            out.extend_from_slice(&t);
        }
        if let Some(th) = thrower {
            out.extend_from_slice(&th);
        }
    }
    let size = (out.len() - start) as u64;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(DROP_OWNERS_MAGIC);
}
fn read_drop_owners_footer(
    payload: &[u8],
) -> Result<(&[u8], Vec<DropOwnerIndex>), &'static str> {
    if !payload.ends_with(DROP_OWNERS_MAGIC) {
        return Ok((payload, Vec::new()));
    }
    let end = payload
        .len()
        .checked_sub(DROP_OWNERS_FOOTER_LEN)
        .ok_or("truncated drop owners footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid drop owners size")?;
    if size == 0 {
        return Err("invalid drop owners size");
    }
    let start = end.checked_sub(size).ok_or("invalid drop owners size")?;
    let mut input = &payload[start..end];
    if input.len() < 4 {
        return Err("truncated drop owners count");
    }
    let count = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
    input = &input[4..];
    if count > input.len() / 5 {
        return Err("invalid drop owners count");
    }
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        if input.len() < 5 {
            return Err("truncated drop owner entry");
        }
        let idx = u32::from_be_bytes(input[..4].try_into().unwrap()) as usize;
        let flags = input[4];
        input = &input[5..];
        if flags == 0 || (flags & !0b11) != 0 {
            return Err("invalid drop owner flags");
        }
        let target = if flags & 1 != 0 {
            if input.len() < 16 {
                return Err("truncated drop target uuid");
            }
            let u: [u8; 16] = input[..16].try_into().unwrap();
            input = &input[16..];
            Some(u)
        } else {
            None
        };
        let thrower = if flags & 2 != 0 {
            if input.len() < 16 {
                return Err("truncated drop thrower uuid");
            }
            let u: [u8; 16] = input[..16].try_into().unwrap();
            input = &input[16..];
            Some(u)
        } else {
            None
        };
        out.push((idx, target, thrower));
    }
    if !input.is_empty() {
        return Err("invalid drop owners tail");
    }
    Ok((&payload[..start], out))
}
fn load_world() -> Result<(), &'static str> {
    load_world_in(&world_save_dir())
}

/// [`load_world`] against one directory: the legacy file, then the companion of every dimension
/// that has one.
fn load_world_in(dir: &std::path::Path) -> Result<(), &'static str> {
    let path = dim_overlay_path(dir, DimId::Overworld);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("failed to read world save"),
    };
    load_world_from(&unwrap_world_payload(raw)?)?;
    for dim in [DimId::Nether, DimId::End] {
        load_dim_overlay_file(dir, dim)?;
    }
    Ok(())
}

/// Load `dim`'s companion overlay when the file exists; a dimension with no file has no edits.
fn load_dim_overlay_file(dir: &std::path::Path, dim: DimId) -> Result<(), &'static str> {
    if dim == DimId::Overworld {
        // The Overworld's edits are the legacy file's leading table, read by `load_world_from`.
        return Ok(());
    }
    let path = dim_overlay_path(dir, dim);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("failed to read world save"),
    };
    persistence_boundary("dimension load panicked", || {
        load_dim_overlay_from(dim, &unwrap_world_payload(raw)?)
    })
}

fn load_world_from(payload: &[u8]) -> Result<(), &'static str> {
    persistence_boundary("world load panicked", || decode_world_payload(payload))
}

fn decode_world_payload(payload: &[u8]) -> Result<(), &'static str> {
    // The optional VER1 record sits just beneath the seed trailer; read it
    // before peeling so a save written for another release is rejected rather
    // than silently parsed under the wrong schema. A file without the record
    // is a pre-header save and means the 1.21.1 header it always meant — which
    // is an exact match only while the running target is 1.21.1 itself.
    let save_version =
        read_save_version_header(payload)?.unwrap_or_else(SaveVersionHeader::legacy);
    let running = running_save_version();
    match classify_save_v2_version(&save_version, running.data_version) {
        SaveVersionCompat::Exact => {}
        SaveVersionCompat::OlderUpgradeable { delta } => {
            eprintln!(
                "[save] refusing to load a world written for {} (DataVersion {}, {delta} versions behind this build's {} / DataVersion {}); run megu-world-migrate to upgrade it",
                save_version.mc_version,
                save_version.data_version,
                running.mc_version,
                running.data_version
            );
            return Err(
                "world snapshot was written for an older Minecraft version; run megu-world-migrate to upgrade it",
            );
        }
        SaveVersionCompat::NewerUnknown { file_dv, running_dv } => {
            eprintln!(
                "[save] refusing to load a world written for {} (DataVersion {file_dv}), newer than this build's {} / DataVersion {running_dv}; run megu-world-migrate to convert it",
                save_version.mc_version, running.mc_version
            );
            return Err(
                "world snapshot was written for a newer or unknown Minecraft version; run megu-world-migrate to convert it",
            );
        }
    }
    *LOADED_SAVE_VERSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(save_version);
    // WBD1 tucks beneath VER1: read it before the peel hides it, and restore
    // it before the body scan so a BRD1 record in the same save keeps owning
    // the live transition.
    let world_border_save = read_world_border_footer(payload)?;
    // Footers are stripped last-written-first.
    let (payload, _) = read_world_seed_footer(payload)?;
    if let Some(border) = world_border_save {
        apply_saved_world_border(&border);
    }
    // MOF1/AER1 tuck between WBE1 and WBD1; footers strip last-written-first.
    let (payload, aer1) = read_mod_bridge_trailer(payload, AERONAUTICS_TRAILER_MAGIC)?;
    let (payload, mof1) = read_mod_bridge_trailer(payload, MOD_OFFLOAD_TRAILER_MAGIC)?;
    let (payload, wbe_entries) = read_wbe_footer(payload)?;
    let (payload, sign_meta_records) = read_sign_meta_footer(payload)?;
    let (payload, dxt) = read_dxt_footer(payload)?;
    let (payload, mut mob_dimensions) = read_mob_dimensions_footer(payload)?;
    let (payload, mut brewing) = read_brewing_footer(payload)?;
    // A payload with no DIM2 footer is the legacy (or DIM1-only) form and parses exactly as before.
    let (payload, dim2) = read_dim2_footer(payload)?;
    let (payload, dim_signs, dim_players) = read_dim_footer(payload)?;
    let (payload, block_loot) = read_block_loot_footer(payload)?;
    let (payload, drop_owners) = read_drop_owners_footer(payload)?;
    let tick_state = tick_persistence::TickState::read(payload)?;
    let before_ticks = tick_persistence::prefix(payload)?;
    let generated = generated_content::Snapshot::read(before_ticks)?;
    let (before_generated, _) = split_save_footer(before_ticks, b"GEN1")?;
    let (payload, body) = split_save_footer(before_generated, SAVE_STATE_MAGIC)?;
    let mut state = body.map(SaveState::read).transpose()?;
    let (payload, advancements) = split_save_footer(payload, ADVANCEMENTS_MAGIC)?;
    let advancements = advancements.map(read_advancements_body).transpose()?;
    let (payload, block_entities) = split_save_footer(payload, BLOCK_ENTITIES_MAGIC)?;
    let mut block_entities = block_entities
        .map(read_block_entities_body)
        .transpose()?;
    if generated.as_ref().is_some_and(|s| (!s.sources.is_empty() || !s.spider_flags.is_empty())
        && worldgen::with_gen(|_| ()).is_none()) {
        return Err("generated content requires initialized world generator");
    }
    if tick_state.as_ref().is_some_and(|s| {
        (!s.sources.is_empty() || !s.generated_blocks.is_empty() || !s.generated_fluids.is_empty())
            && worldgen::with_gen(|_| ()).is_none()
    }) {
        return Err("tick state requires initialized world generator");
    }
    let mut overlay = SaveInput(payload);
    let chunks = read_dim_overlay(DimId::Overworld, &mut overlay)?;
    let mut p = payload.len() - overlay.0.len();
    let b = payload;
    let rv = rv_varint;
    let has_mob_dimensions = !mob_dimensions.is_empty();
    let has_meta = matches!(payload.get(p..p + 4), Some(b"MET2" | b"MET3" | b"MET4"));
    if (has_mob_dimensions || state.is_some()) && !has_meta {
        return Err("mob dimension count does not match mob records");
    }
    // A legacy MET4 section can restore completed IDs. Clear previous-world
    // grants before reading it, then replace them with authoritative ADV1 data.
    adv_done().lock().unwrap_or_else(|e| e.into_inner()).clear();
    // optional META tail
    let mut restored_mobs = Vec::new();
    let mut restored_sheared = Vec::new();
    let mut restored_mob_tags = Vec::new();
    if has_meta {
        let magic = &payload[p..p + 4];
        let has_comps = magic != b"MET2";
        // MET4 stores entity ages as server ticks. MET2 and MET3 stored them as
        // milliseconds, and one millisecond is not one tick, so a legacy file
        // read as ticks would despawn its drops immediately.
        let legacy_ms_ages = magic != b"MET4";
        let mut mob_input = SaveInput(&b[p + 4..]);
        let mn = mob_input.var_count(29)?;
        if mn > MOB_CAP {
            return Err("invalid mob count");
        }
        if has_mob_dimensions && mn != mob_dimensions.len() {
            return Err("mob dimension count does not match mob records");
        }
        for _ in 0..mn {
            let etype = mob_input.varint()?;
            let (x, y, z) = mob_input.position()?;
            let hp = mob_input.f32()?;
            restored_mobs.push((etype, x, y, z, hp));
        }
        p = b.len() - mob_input.0.len();
        if state.as_ref().is_some_and(|state| {
            magic != b"MET4"
                || state.mobs.len() != mn
                || state
                    .mobs
                    .iter()
                    .zip(&restored_mobs)
                    .any(|(extra, legacy)| extra.etype != legacy.0)
        }) {
            return Err("saved mob state does not match mob records");
        }
        if p + 8 <= payload.len() {
            WORLD_TIME.store(
                u64::from_be_bytes(b[p..p + 8].try_into().unwrap()),
                Ordering::Relaxed,
            );
            p += 8;
        }
        if p < payload.len() {
            let pn = rv(payload, &mut p);
            let mut store = inv_store().lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..pn {
                if p + 2 > payload.len() {
                    break;
                }
                let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                p += 2;
                if p + nl > payload.len() {
                    break;
                }
                let name = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                p += nl;
                let en = rv(payload, &mut p);
                let mut inv = HashMap::new();
                for _ in 0..en {
                    if p + 3 > payload.len() {
                        break;
                    }
                    let slot = i16::from_be_bytes(b[p..p + 2].try_into().unwrap());
                    p += 2;
                    let item = rv(payload, &mut p);
                    if p >= payload.len() {
                        break;
                    }
                    let cnt = b[p];
                    p += 1;
                    let comps = if has_comps {
                        read_comps(payload, &mut p)
                    } else {
                        Vec::new()
                    };
                    inv.insert(slot, (item, cnt, comps));
                }
                store.insert(name, inv);
            }
        }
        if p < payload.len() {
            let cn = rv(payload, &mut p);
            let mut ch = chests().lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..cn {
                if p + 12 > payload.len() {
                    break;
                }
                let x = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let y = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let z = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let slots = read_stack_table(payload, &mut p, has_comps);
                // The legacy chest section has no dimension byte: every entry in it is Overworld.
                ch.insert(ContainerKey::Block((DimId::Overworld, x, y, z)), slots);
            }
        }
        if p < payload.len() {
            let sn = rv(payload, &mut p);
            let mut sg = signs().lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..sn {
                if p + 13 > payload.len() {
                    break;
                }
                let x = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let y = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let z = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let front = b[p];
                p += 1;
                let mut lines: [String; 4] = Default::default();
                for l in lines.iter_mut() {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + nl > payload.len() {
                        break;
                    }
                    *l = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                    p += nl;
                }
                // The legacy sign section has no dimension byte: every sign in it is Overworld.
                sg.insert((DimId::Overworld, x, y, z, front), lines);
            }
        }
        if p < payload.len() {
            let fn_ = rv(payload, &mut p);
            let mut fl = forceloaded().lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..fn_ {
                if p + 8 > payload.len() {
                    break;
                }
                let cx = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let cz = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                fl.insert((cx, cz));
            }
        }
        if p < payload.len() {
            let pn = rv(payload, &mut p);
            let mut ps = pos_store().lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..pn {
                if p + 2 > payload.len() {
                    break;
                }
                let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                p += 2;
                if p + nl + 28 > payload.len() {
                    break;
                }
                let name = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                p += nl;
                let x = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let y = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let z = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let yaw = f32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let pitch = f32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                // The legacy position section has no dimension byte: every player defaulted to
                // the Overworld; a footer entry below corrects the few who logged out elsewhere.
                ps.insert(name, (DimId::Overworld, x, y, z, yaw, pitch));
            }
        }
        if p < payload.len() {
            let dn = rv(payload, &mut p);
            let mut ds = drops().lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..dn {
                if p + 30 > payload.len() {
                    break;
                }
                let eid = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let x = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let y = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let z = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let item = rv(payload, &mut p);
                if p >= payload.len() {
                    break;
                }
                let cnt = b[p];
                p += 1;
                let comps = if has_comps {
                    read_comps(payload, &mut p)
                } else {
                    Vec::new()
                };
                if eid >= NEXT_EID.load(Ordering::Relaxed) {
                    NEXT_EID.store(eid + 1, Ordering::SeqCst);
                }
                ds.push(Drop {
                    eid,
                    x,
                    y,
                    z,
                    item,
                    cnt,
                    comps,
                    age: 0,
                    target: None,
                    thrower: None,
                });
            }
            for &(idx, target, thrower) in &drop_owners {
                if let Some(d) = ds.get_mut(idx) {
                    d.target = target;
                    d.thrower = thrower;
                }
            }
        }
        if p < payload.len() {
            let fn_ = rv(payload, &mut p);
            let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..fn_ {
                if p + 12 > payload.len() {
                    break;
                }
                let x = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let y = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let z = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let Some(fr) = read_furnace_state(payload, &mut p, has_comps, legacy_ms_ages)
                else {
                    break;
                };
                // The legacy furnace section has no dimension byte: every entry is Overworld.
                fs.insert((DimId::Overworld, x, y, z), fr);
            }
            eprintln!("[spine] loaded {} furnaces", fs.len());
        }
        if p < payload.len() {
            // sheared mob indices -> mark by load order (fresh eids)
            let sn = rv(payload, &mut p);
            for _ in 0..sn {
                restored_sheared.push(rv(payload, &mut p) as usize);
            }
        }
        if p < payload.len() {
            let on = rv(payload, &mut p);
            let mut board = scoreboard().lock().unwrap_or_else(|e| e.into_inner());
            for _ in 0..on {
                if p + 2 > payload.len() {
                    break;
                }
                let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                p += 2;
                if p + nl > payload.len() {
                    break;
                }
                let obj = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                p += nl;
                let en = rv(payload, &mut p);
                let mut entries = HashMap::new();
                for _ in 0..en {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let el = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + el + 8 > payload.len() {
                        break;
                    }
                    let entry = String::from_utf8_lossy(&payload[p..p + el]).to_string();
                    p += el;
                    let val = i64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                    p += 8;
                    entries.insert(entry, ScoreboardScore {
                        value: val.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                        trigger_enabled: false,
                    });
                }
                board.scores.insert(obj.clone(), entries);
                board.objectives.insert(obj.clone(), ScoreboardObjective {
                    display_name: obj.clone(),
                    name: obj,
                    criteria: ScoreboardCriteria::Dummy,
                    render_type: ScoreboardRenderType::Integer,
                });
            }
        }
        if p + 1 < payload.len() {
            // XP orbs (eid,x,y,z,val,age_ms)
            let xn = rv(payload, &mut p);
            let mut xs = xps().lock().unwrap_or_else(|e| e.into_inner());
            let mut mx: i32 = 0;
            for _ in 0..xn {
                if p + 34 > payload.len() {
                    break;
                }
                let eid = i32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let x = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let y = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let z = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                p += 8;
                let val = i16::from_be_bytes(b[p..p + 2].try_into().unwrap());
                p += 2;
                let ams = u32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let age = legacy_ticks(u64::from(ams), legacy_ms_ages);
                xs.push(Xp {
                    eid,
                    x,
                    y,
                    z,
                    val,
                    age: age.min(server_ticks(XP_LIFE_GT)) as i32,
                });
                if eid > mx {
                    mx = eid;
                }
            }
            if mx > 0 {
                NEXT_EID.fetch_max(mx + 1, Ordering::SeqCst);
            }
            if !xs.is_empty() {
                eprintln!("[spine] loaded {} xp orbs", xs.len());
            }
        }
        if p < payload.len() {
            // drop ages by order — restore remaining despawn budget
            let mut ds = drops().lock().unwrap_or_else(|e| e.into_inner());
            for d in ds.iter_mut() {
                if p + 4 > payload.len() {
                    break;
                }
                let raw = u32::from_be_bytes(b[p..p + 4].try_into().unwrap());
                p += 4;
                let aticks = legacy_ticks(u64::from(raw), legacy_ms_ages);
                // Reinterpret as signed so a negative pickup-delay marker survives
                // instead of wrapping to a value that clamps to a full lifespan.
                d.age = aticks as i32;
            }
        }
        if p < payload.len() {
            let rf3 = |b: &[u8], p: &mut usize| -> f64 {
                let v = f64::from_be_bytes(b[*p..*p + 8].try_into().unwrap());
                *p += 8;
                v
            };
            let ri4 = |b: &[u8], p: &mut usize| -> i32 {
                let v = i32::from_be_bytes(b[*p..*p + 4].try_into().unwrap());
                *p += 4;
                v
            };
            let mut mx = 0i32;
            let bump = |eid: i32, mx: &mut i32| {
                if eid > *mx {
                    *mx = eid;
                }
            };
            let arw2 = b[p..].starts_with(b"ARW2");
            if arw2 {
                p += 4;
            }
            let n = rv(payload, &mut p);
            {
                let mut ar = arrows().lock().unwrap_or_else(|e| e.into_inner());
                let rec = if arw2 { 61 } else { 60 };
                for _ in 0..n {
                    if p + rec > payload.len() {
                        break;
                    }
                    let eid = ri4(payload, &mut p);
                    let owner = ri4(payload, &mut p);
                    let (x, y, z) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let (dx, dy, dz) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let ttl = legacy_tick_count(ri4(payload, &mut p) as u64, legacy_ms_ages) as i32;
                    let stuck = if arw2 {
                        let v = b[p] != 0;
                        p += 1;
                        v
                    } else {
                        false
                    };
                    ar.push(Arrow {
                        eid,
                        owner,
                        x,
                        y,
                        z,
                        dx,
                        dy,
                        dz,
                        ttl,
                        dmg: 6.0,
                        fire: 0,
                        punch: 0,
                        kind: 0,
                        loy: 0,
                        ret_item: 0,
                        ret_comps: Vec::new(),
                        pierce: 0,
                        last_hit: -1,
                        stuck,
                    });
                    bump(eid, &mut mx);
                }
            }
            let n = rv(payload, &mut p);
            {
                let mut bo = bobbers().lock().unwrap_or_else(|e| e.into_inner());
                for _ in 0..n {
                    if p + 61 > payload.len() {
                        break;
                    }
                    let eid = ri4(payload, &mut p);
                    let owner = ri4(payload, &mut p);
                    let (x, y, z) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let (dx, dy, dz) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let age = ri4(payload, &mut p);
                    let bit = b[p] != 0;
                    p += 1;
                    bo.push(Bobber {
                        eid,
                        owner,
                        x,
                        y,
                        z,
                        dx,
                        dy,
                        dz,
                        age,
                        bit,
                    });
                    bump(eid, &mut mx);
                }
            }
            let n = rv(payload, &mut p);
            {
                let mut pe = pearls().lock().unwrap_or_else(|e| e.into_inner());
                for _ in 0..n {
                    if p + 60 > payload.len() {
                        break;
                    }
                    let eid = ri4(payload, &mut p);
                    let owner = ri4(payload, &mut p);
                    let (x, y, z) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let (dx, dy, dz) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let ttl = legacy_tick_count(ri4(payload, &mut p) as u64, legacy_ms_ages) as i32;
                    pe.push(Pearl {
                        eid,
                        owner,
                        x,
                        y,
                        z,
                        dx,
                        dy,
                        dz,
                        ttl,
                    });
                    bump(eid, &mut mx);
                }
            }
            let n = rv(payload, &mut p);
            {
                let mut pr = primed().lock().unwrap_or_else(|e| e.into_inner());
                for _ in 0..n {
                    if p + 32 > payload.len() {
                        break;
                    }
                    let eid = ri4(payload, &mut p);
                    let (x, y, z) = (
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                        rf3(payload, &mut p),
                    );
                    let left = legacy_tick_count(
                        i32::from_be_bytes(b[p..p + 4].try_into().unwrap()) as u64,
                        legacy_ms_ages,
                    ) as i32;
                    p += 4;
                    pr.push(Primed { eid, x, y, z, left });
                    bump(eid, &mut mx);
                }
            }
            if mx > 0 {
                NEXT_EID.fetch_max(mx + 1, Ordering::SeqCst);
            }
        }
        if p + 8 <= payload.len() {
            RAINING.store(
                legacy_tick_count(
                    u64::from_be_bytes(b[p..p + 8].try_into().unwrap()),
                    legacy_ms_ages,
                ),
                Ordering::Relaxed,
            );
            p += 8;
        }
        if p + 8 <= payload.len() {
            THUNDERING.store(
                legacy_tick_count(
                    u64::from_be_bytes(b[p..p + 8].try_into().unwrap()),
                    legacy_ms_ages,
                ),
                Ordering::Relaxed,
            );
            p += 8;
        }
        if p + 4 <= payload.len() {
            for r in [
                &GR_KEEP_INVENTORY,
                &GR_DAYLIGHT_CYCLE,
                &GR_WEATHER_CYCLE,
                &GR_MOB_SPAWNING,
                &GR_MOB_GRIEFING,
                &GR_NATURAL_REGEN,
                &GR_MOB_LOOT,
            ] {
                r.store(b[p], Ordering::Relaxed);
                p += 1;
            }
        }
        if p + 25 <= payload.len() {
            let (x, y, z) = (
                f64::from_be_bytes(b[p..p + 8].try_into().unwrap()),
                f64::from_be_bytes(b[p + 8..p + 16].try_into().unwrap()),
                f64::from_be_bytes(b[p + 16..p + 24].try_into().unwrap()),
            );
            *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = (x, y, z, 0.0);
            p += 24;
            DIFFICULTY.store(b[p].min(3), Ordering::Relaxed);
            p += 1;
        }
        if p < payload.len() {
            let mut tm = tags().lock().unwrap_or_else(|e| e.into_inner());
            let pn = rv(payload, &mut p);
            for _ in 0..pn {
                if p + 2 > payload.len() {
                    break;
                }
                let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                p += 2;
                if p + nl > payload.len() {
                    break;
                }
                let nm = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                p += nl;
                let tc = rv(payload, &mut p);
                let mut st = HashSet::new();
                for _ in 0..tc {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let tl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + tl > payload.len() {
                        break;
                    }
                    st.insert(String::from_utf8_lossy(&payload[p..p + tl]).to_string());
                    p += tl;
                }
                if !st.is_empty() {
                    tm.insert(format!("p:{}", nm), st);
                }
            }
            let mn2 = rv(payload, &mut p);
            let mut mtag: Vec<(usize, HashSet<String>)> = Vec::new();
            for _ in 0..mn2 {
                let i = rv(payload, &mut p) as usize;
                let tc = rv(payload, &mut p);
                let mut st = HashSet::new();
                for _ in 0..tc {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let tl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + tl > payload.len() {
                        break;
                    }
                    st.insert(String::from_utf8_lossy(&payload[p..p + tl]).to_string());
                    p += tl;
                }
                if !st.is_empty() {
                    mtag.push((i, st));
                }
            }
            restored_mob_tags = mtag;
            if p < payload.len() {
                let sn = rv(payload, &mut p);
                let mut sf = sched_fns().lock().unwrap_or_else(|e| e.into_inner());
                let t_now = TICK.load(Ordering::Relaxed);
                for _ in 0..sn {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + nl > payload.len() {
                        break;
                    }
                    let f2 = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                    p += nl;
                    if p + 8 > payload.len() {
                        break;
                    }
                    let rem = u64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                    p += 8;
                    sf.push((f2, t_now + rem));
                }
            }
            if p < payload.len() {
                let gn = rv(payload, &mut p);
                let mut ag = adv_done().lock().unwrap_or_else(|e| e.into_inner());
                for _ in 0..gn {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + nl > payload.len() {
                        break;
                    }
                    let pn = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                    p += nl;
                    let ic = rv(payload, &mut p);
                    let mut ids = HashSet::new();
                    for _ in 0..ic {
                        if p + 2 > payload.len() {
                            break;
                        }
                        let il = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                        p += 2;
                        if p + il > payload.len() {
                            break;
                        }
                        ids.insert(String::from_utf8_lossy(&payload[p..p + il]).to_string());
                        p += il;
                    }
                    if !ids.is_empty() {
                        ag.insert(pn, ids);
                    }
                }
            }
            // player attributes
            if p < payload.len() {
                let an = rv(payload, &mut p);
                let mut at = attrs().lock().unwrap_or_else(|e| e.into_inner());
                for _ in 0..an {
                    if p + 2 > payload.len() {
                        break;
                    }
                    let nl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                    p += 2;
                    if p + nl > payload.len() {
                        break;
                    }
                    let nm = String::from_utf8_lossy(&payload[p..p + nl]).to_string();
                    p += nl;
                    let cnt = rv(payload, &mut p);
                    let mut m = HashMap::new();
                    for _ in 0..cnt {
                        if p + 2 > payload.len() {
                            break;
                        }
                        let kl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                        p += 2;
                        if p + kl + 8 > payload.len() {
                            break;
                        }
                        let kn = String::from_utf8_lossy(&payload[p..p + kl]).to_string();
                        p += kl;
                        let v = f64::from_be_bytes(b[p..p + 8].try_into().unwrap());
                        p += 8;
                        m.insert(kn, v);
                    }
                    if !m.is_empty() {
                        at.insert(format!("p:{}", nm), m);
                    }
                }
                // objective types + sidebar
                if p < payload.len() {
                    let oc = rv(payload, &mut p);
                    let mut board = scoreboard().lock().unwrap_or_else(|e| e.into_inner());
                    for _ in 0..oc {
                        if p + 2 > payload.len() {
                            break;
                        }
                        let ol = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                        p += 2;
                        if p + ol + 2 > payload.len() {
                            break;
                        }
                        let on = String::from_utf8_lossy(&payload[p..p + ol]).to_string();
                        p += ol;
                        let tl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                        p += 2;
                        if p + tl > payload.len() {
                            break;
                        }
                        let criterion = String::from_utf8_lossy(&payload[p..p + tl]);
                        if let (Some(objective), Some(criterion)) =
                            (board.objectives.get_mut(&on), ScoreboardCriteria::parse(&criterion))
                        {
                            objective.criteria = criterion;
                            if criterion == ScoreboardCriteria::Health {
                                objective.render_type = ScoreboardRenderType::Hearts;
                            }
                        }
                        p += tl;
                    }
                    if p + 2 <= payload.len() {
                        let sl = u16::from_be_bytes(b[p..p + 2].try_into().unwrap()) as usize;
                        p += 2;
                        if p + sl <= payload.len() && sl > 0 {
                            let obj = String::from_utf8_lossy(&payload[p..p + sl]);
                            let _ = board.set_display(ScoreboardDisplaySlot::Sidebar, Some(&obj));
                            p += sl;
                        }
                    }
                }
            }
        }
        eprintln!(
            "[spine] loaded meta: {} mobs, {} invs, {} chests",
            mn,
            inv_store().lock().unwrap_or_else(|e| e.into_inner()).len(),
            chests().lock().unwrap_or_else(|e| e.into_inner()).len()
        );
    }

    apply_dim_overlay(chunks);

    // Apply the non-Overworld overrides after the legacy sections have loaded. A sign whose key
    // already exists in the Overworld section stays independent: the footer's key names its own
    // dimension. A player's coordinates came from the legacy section; only the dimension changes.
    if !dim_signs.is_empty() {
        let mut sg = signs().lock().unwrap_or_else(|e| e.into_inner());
        for (dim, x, y, z, front, lines) in dim_signs {
            sg.insert((dim, x, y, z, front), lines);
        }
    }
    if !dim_players.is_empty() {
        let mut ps = pos_store().lock().unwrap_or_else(|e| e.into_inner());
        for (name, dim) in dim_players {
            if let Some(entry) = ps.get_mut(&name) {
                entry.0 = dim;
            }
        }
    }

    // Falling queue, when the save carries one. Absent sections are the normal case for saves
    // written before this existed, so a missing tag leaves the queue empty rather than failing.
    if p + 4 <= payload.len() && &payload[p..p + 4] == b"FAL2" {
        p += 4;
        let n = rv(payload, &mut p);
        for _ in 0..n {
            if p + 44 > payload.len() {
                break;
            }
            let eid = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            let x = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap());
            p += 8;
            let y = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap());
            p += 8;
            let z = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap());
            p += 8;
            let dy = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap());
            p += 8;
            let state = u32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            let time = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            NEXT_EID.fetch_max(eid + 1, Ordering::SeqCst);
            falling().lock().unwrap_or_else(|e| e.into_inner()).push(Fall {
                eid,
                x,
                y,
                z,
                dy,
                state,
                time,
            });
        }
    } else if p + 4 <= payload.len() && &payload[p..p + 4] == b"FAL1" {
        // The old cell-moving model kept the block in the world and stored its cell. Convert to
        // an entity and remove the block, so it does not remain after the entity lands.
        p += 4;
        let n = rv(payload, &mut p);
        for _ in 0..n {
            if p + 12 > payload.len() {
                break;
            }
            let fx = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            let fy = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            let fz = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            let st = block_at(fx, fy, fz);
            if st == 0 {
                continue;
            }
            // An absent overlay entry falls back to generated terrain, so removing the entry
            // would leave a generated block where the migrating block used to be. Record air
            // explicitly: the block is now the entity, not whatever terrain is underneath it.
            world()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry((DimId::Overworld, fx >> 4, fz >> 4))
                .or_default()
                .insert(local_idx(fx, fy, fz), 0);
            let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
            falling().lock().unwrap_or_else(|e| e.into_inner()).push(Fall {
                eid,
                x: fx as f64 + 0.5,
                y: fy as f64,
                z: fz as f64 + 0.5,
                dy: 0.0,
                state: st,
                time: 0,
            });
        }
    }

    GR_MAX_ENTITY_CRAMMING.store(ENTITY_CRAMMING_DEFAULT, Ordering::Relaxed);
    // World random state, when the save carries one. A save without it (written before
    // this section existed) leaves the stream at the seed-derived start.
    if p + 4 <= payload.len() && &payload[p..p + 4] == b"RNG1" {
        p += 4;
        if p + 8 <= payload.len() {
            world_rng::restore(u64::from_be_bytes(payload[p..p + 8].try_into().unwrap()));
            p += 8;
        }
    }
    if p + 4 <= payload.len() && &payload[p..p + 4] == b"GCR1" {
        p += 4;
        if p + 4 <= payload.len() {
            GR_MAX_ENTITY_CRAMMING.store(
                i32::from_be_bytes(payload[p..p + 4].try_into().unwrap()),
                Ordering::Relaxed,
            );
            p += 4;
        }
    }
    if p + 4 <= payload.len() && &payload[p..p + 4] == b"BRD1" {
        p += 4;
        const BORDER_SAVE_LEN: usize = 8 * 7 + 4 * 3;
        if p + BORDER_SAVE_LEN <= payload.len() {
            let center_x = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let center_z = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let old_size = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let new_size = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let duration = i64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let damage_per_block = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let damage_safe_zone = f64::from_be_bytes(payload[p..p + 8].try_into().unwrap()); p += 8;
            let warning_blocks = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap()); p += 4;
            let warning_time_secs = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap()); p += 4;
            let absolute_max_size = i32::from_be_bytes(payload[p..p + 4].try_into().unwrap());
            p += 4;
            if [center_x, center_z, old_size, new_size, damage_per_block, damage_safe_zone]
                .iter().all(|value| value.is_finite())
            {
                *world_border().lock().unwrap_or_else(|e| e.into_inner()) = WorldBorderState {
                    center_x, center_z, old_size, new_size,
                    lerp_start_ms: border_now_ms(),
                    lerp_duration_ms: duration.max(0),
                    damage_per_block, damage_safe_zone, warning_blocks,
                    warning_time_secs, absolute_max_size,
                };
            }
        }
    }
    if let Some(bytes) = payload.get(p..p + 8).filter(|bytes| &bytes[..4] == b"SPA1") {
        let angle = f32::from_be_bytes(bytes[4..].try_into().unwrap());
        if angle.is_finite() {
            WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()).3 = angle;
        }
    }
    if let Some(state) = tick_state {
        TICK_IDX.store(state.now, Ordering::Relaxed);
        BLOCK_TICK_ORDER.store(state.block_order, Ordering::Relaxed);
        *block_ticks().lock().unwrap_or_else(|e| e.into_inner()) = state.blocks;
        let mut owner = GeneratedFluidOwner::new();
        owner.queue = state.fluids;
        owner.next_order = state.fluid_order;
        owner.owned = state.owned;
        *GENERATED_FLUID_OWNER
            .get_or_init(|| Mutex::new(GeneratedFluidOwner::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = owner;
        worldgen::with_gen(|g| {
            g.restore_tick_sources(state.sources);
            g.restore_generated_ticks(state.generated_blocks, state.generated_fluids);
        });
    }
    if let Some(mut generated) = generated {
        let mut cart_inventories = Vec::new();
        for cart in &mut generated.content.carts {
            cart.eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
            if !cart.has_loot_table() {
                let uuid = cart.uuid()?;
                if let crate::nbt::Tag::Compound(fields) = &mut cart.data {
                    if let Some(items) = fields.remove(&crate::nbt::NbtString::text("Items")) {
                        cart_inventories.push((ContainerKey::Cart(uuid), items_nbt_to_stacks(&items)?));
                    }
                }
            }
        }
        for frame in &mut generated.content.frames {
            frame.eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
        }
        *generated_content().lock().unwrap_or_else(|e| e.into_inner()) = generated.content;
        if !cart_inventories.is_empty() {
            let mut ch = chests().lock().unwrap_or_else(|e| e.into_inner());
            for (key, slots) in cart_inventories {
                ch.insert(key, slots);
            }
        }
        worldgen::with_gen(|g| {
            g.payload_sources = generated.sources;
            g.mineshaft_spawners = generated.spider_flags;
        });
    }
    // The legacy block-loot footer is Overworld-only; its positions load back with that dimension.
    *pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()) = block_loot
        .into_iter()
        .map(|(pos, entry)| (ow_block(pos), entry))
        .collect();
    // DIM2 entries restore into their own dimensions. They are applied after the legacy sections
    // so a mixed save cannot let an old Overworld entry overwrite a new one at the same coordinate.
    if !dim2.is_empty() {
        {
            let mut ch = chests().lock().unwrap_or_else(|e| e.into_inner());
            for (pos, slots) in dim2.chests {
                ch.insert(ContainerKey::Block(pos), slots);
            }
        }
        {
            let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            for (pos, fr) in dim2.furnaces {
                fs.insert(pos, fr);
            }
        }
        {
            let mut cg = crops().lock().unwrap_or_else(|e| e.into_inner());
            for (pos, age) in dim2.crops {
                cg.insert(pos, age);
            }
        }
        {
            let mut all = pending_block_loot().lock().unwrap_or_else(|e| e.into_inner());
            for (pos, entry) in dim2.loot {
                all.insert(pos, entry);
            }
        }
    }
    // Entity-scoped attributes belong to the loaded world, including when it
    // contains no mobs. Old runtime EIDs must not retain stale values.
    attrs().lock().unwrap_or_else(|e| e.into_inner()).retain(|key, _| !key.starts_with("e:"));
    // Persisted drop, projectile, TNT and falling-block IDs must reserve their range
    // before mobs receive fresh IDs. Keep mob ordinals stable for shearing and tags.
    if !restored_mobs.is_empty() {
        let mob_attributes = state.as_mut()
            .map(|s| std::mem::take(&mut s.mob_attributes))
            .unwrap_or_default();
        let extra = state
            .as_mut()
            .map(|s| std::mem::take(&mut s.mobs))
            .unwrap_or_default();
        // Live-created mobs derive their UUID from the entity ID. Keep new
        // spawns from reusing a UUID already present in the loaded world.
        for mob in &extra {
            if let [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, a, b, c, d] = mob.uuid {
                let id = i32::from_be_bytes([a, b, c, d]);
                if let Some(next) = id.checked_add(1) {
                    NEXT_EID.fetch_max(next, Ordering::SeqCst);
                }
            }
        }
        let mut extra = extra.into_iter();
        let mut ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
        let first = ms.len();
        for (index, (etype, x, y, z, hp)) in restored_mobs.into_iter().enumerate() {
            let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
            let mut uuid = [0; 16];
            uuid[12..].copy_from_slice(&eid.to_be_bytes());
            let mut rotation = (0.0, 0.0);
            let mut vel = [0.0; 3];
            let (mut fuse, mut fire_ticks, mut love) = (0, 0, 0);
            let mut air = server_ticks(AIR_SUPPLY_GT) as i32;
            if let Some(mut saved) = extra.next() {
                uuid = saved.uuid;
                rotation = (saved.yaw, saved.pitch);
                vel = saved.vel;
                (fuse, fire_ticks, love, air) = (saved.fuse, saved.fire_ticks, saved.love, saved.air);
                let now = TICK.load(Ordering::Relaxed);
                for (_, until) in &mut saved.state.potions {
                    *until = now.saturating_add(*until);
                }
                if saved.state.behavior.is_some()
                    || saved.state.blaze_charged
                    || !saved.state.potions.is_empty()
                {
                    live_mob_states().lock().unwrap_or_else(|e| e.into_inner()).insert(eid, saved.state);
                }
                if saved.sitting_cat {
                    sitting_cats().lock().unwrap_or_else(|e| e.into_inner()).insert(eid);
                }
                if let Some(villager) = saved.villager {
                    live_villagers().lock().unwrap_or_else(|e| e.into_inner()).insert(eid, villager);
                }
            }
            let last_action = Instant::now() - Duration::from_secs(10);
            ms.push(Arc::new(Mob {
                dim: mob_dimensions.get(index).copied().unwrap_or(DimId::Overworld),
                eid,
                uuid,
                etype,
                pos: Mutex::new((x, y, z, rotation.0, rotation.1)),
                hp: Mutex::new(hp),
                last_hit: Mutex::new(last_action),
                hurt_at: Mutex::new(last_action),
                last_hurt_tick: Mutex::new(None),
                vel: Mutex::new(vel),
                fuse: Mutex::new(fuse),
                fire_ticks: Mutex::new(fire_ticks),
                love: Mutex::new(love),
                air: Mutex::new(air),
            }));
        }
        let mut attributes = attrs().lock().unwrap_or_else(|e| e.into_inner());
        for (index, selector_key, values) in mob_attributes {
            let prefix = if selector_key { "e:m:" } else { "e:" };
            attributes.insert(format!("{}{}", prefix, ms[first + index].eid), values);
        }
        let mut sh = sheared().lock().unwrap_or_else(|e| e.into_inner());
        for index in restored_sheared {
            if let Some(m) = ms.get(first + index) {
                sh.insert(m.eid);
            }
        }
        drop(sh);
        let mut tm = tags().lock().unwrap_or_else(|e| e.into_inner());
        for (index, state) in restored_mob_tags {
            if let Some(m) = ms.get(first + index) {
                tm.insert(format!("e:{}", m.eid), state);
            }
        }
    }
    let mut state = state.unwrap_or_default();
    if !dxt.is_empty() {
        let dxt = &dxt;
        // Resolve every named record against the registry this process is
        // running — a datapack added, removed, or reordered between restarts
        // changes slots, never names.
        for (key, bytes) in &dxt.overlays {
            if let Some(dim) = dxt_dim(key) {
                if let Err(msg) = load_dim_overlay_from(dim, bytes) {
                    eprintln!("[spine] {key} overlay: {msg}");
                }
            }
        }
        {
            let mut ps = pos_store().lock().unwrap_or_else(|e| e.into_inner());
            for (name, key) in &dxt.players {
                if let (Some(dim), Some(entry)) = (dxt_dim(key), ps.get_mut(name)) {
                    entry.0 = dim;
                }
            }
        }
        for (uuid, position, respawn) in &dxt.saved_players {
            if let Some(player) = state.players.get_mut(uuid) {
                if let Some(key) = position {
                    if let Some(dim) = dxt_dim(key) {
                        player.position.0 = dim;
                    }
                }
                if let Some(key) = respawn {
                    if let Some(dim) = dxt_dim(key) {
                        if let Some(point) = &mut player.respawn {
                            point.dim = dim;
                        }
                    }
                }
            }
        }
        for (ordinal, key) in &dxt.mob_dims {
            if let (Some(dim), Some(slot)) = (dxt_dim(key), mob_dimensions.get_mut(*ordinal as usize)) {
                *slot = dim;
            }
        }
        let raids = &mut state.raids;
        for (ordinal, key) in &dxt.raid_dims {
            if let (Some(dim), Some(raid)) = (dxt_dim(key), raids.get_mut(*ordinal as usize)) {
                raid.0 = dim;
            }
        }
        {
            let mut sg = signs().lock().unwrap_or_else(|e| e.into_inner());
            for (key, x, y, z, front, lines) in &dxt.signs {
                if let Some(dim) = dxt_dim(key) {
                    sg.insert((dim, *x, *y, *z, *front), lines.clone());
                }
            }
        }
        {
            let mut ch = chests().lock().unwrap_or_else(|e| e.into_inner());
            for (key, x, y, z, items) in &dxt.chests {
                if let Some(dim) = dxt_dim(key) {
                    ch.insert(ContainerKey::Block((dim, *x, *y, *z)), items.clone());
                }
            }
        }
        {
            let mut fs = furnaces().lock().unwrap_or_else(|e| e.into_inner());
            for (key, x, y, z, furnace) in &dxt.furnaces {
                if let Some(dim) = dxt_dim(key) {
                    fs.insert((dim, *x, *y, *z), clone_furnace_for_footer(furnace));
                }
            }
        }
        {
            let mut cg = crops().lock().unwrap_or_else(|e| e.into_inner());
            for (key, x, y, z, age) in &dxt.crops {
                if let Some(dim) = dxt_dim(key) {
                    cg.insert((dim, *x, *y, *z), *age);
                }
            }
        }
        for (key, x, y, z, loot) in &dxt.loot {
            if let Some(dim) = dxt_dim(key) {
                pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()).insert((dim, *x, *y, *z), loot.clone());
            }
        }
        for (key, x, y, z, slots, fuel, remaining) in &dxt.brewing {
            if let Some(dim) = dxt_dim(key) {
                let mut entry = LiveBrewingStand::default();
                for (index, stack) in slots {
                    if let Some(slot) = entry.menu.slots.get_mut(*index as usize) {
                        *slot = stack.clone();
                    }
                }
                entry.stand.fuel = *fuel;
                entry.stand.brew_time = *remaining;
                if *remaining > 0 {
                    entry.stand.active_ingredient =
                        item_to_name(entry.menu.slots[3].0).and_then(BrewingIngredient::from_item);
                }
                brewing.insert((dim, *x, *y, *z), entry);
            }
        }
        let mut custom_entities = Vec::with_capacity(dxt.block_entities.len());
        for (key, x, y, z, state_id, tag) in &dxt.block_entities {
            if let Some(dim) = dxt_dim(key) {
                custom_entities.push(((dim, *x, *y, *z), *state_id, tag.clone()));
            }
        }
        block_entities = Some(
            block_entities
                .take()
                .unwrap_or_default()
                .into_iter()
                .chain(custom_entities)
                .collect(),
        );
    }
    // SGN1 records name their dimension by canonical key like DXT1's, so they
    // resolve against the same registry this process is running.
    if !sign_meta_records.is_empty() {
        let mut meta = sign_meta().lock().unwrap_or_else(|e| e.into_inner());
        for (key, x, y, z, record) in sign_meta_records {
            if let Some(dim) = dxt_dim(&key) {
                meta.insert((dim, x, y, z), record);
            }
        }
    }
    if let Some(saved) = advancements {
        *adv_done().lock().unwrap_or_else(|e| e.into_inner()) = saved.granted;
        *player_advancements().lock().unwrap_or_else(|e| e.into_inner()) = saved.core;
        *adv_prog().lock().unwrap_or_else(|e| e.into_inner()) = saved.datapack;
    } else {
        *player_advancements().lock().unwrap_or_else(|e| e.into_inner()) = AdvancementTracker::default();
        adv_prog().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    restore_gamerules(state.gamerules.as_deref());
    *WORLD_SPAWN_PITCH.lock().unwrap_or_else(|e| e.into_inner()) = state.spawn_pitch;
    *saved_players().lock().unwrap_or_else(|e| e.into_inner()) = state.players;
    *saved_raids().lock().unwrap_or_else(|e| e.into_inner()) = state.raids;
    *live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner()) = brewing;
    restore_wbe_entries(wbe_entries);
    restore_saved_block_entities(block_entities);
    // The mod-bridge trailers apply last: their codecs validate the whole blob
    // before touching live state, and running them after the core world is
    // fully restored means a corrupt trailer can never observe — or leave —
    // half-loaded core state.
    apply_mod_bridge_trailers(mof1, aer1)?;
    eprintln!(
        "[spine] loaded {} dirty chunks",
        world().lock().unwrap_or_else(|e| e.into_inner()).len()
    );
    rebuild_derived_indexes();
    Ok(())
}

const BREWING_SAVE_MAGIC: &[u8; 4] = b"BRS1";

/// The live brewing stands a custom dimension owns, for DXT1. The BRS1 footer
/// keeps its vanilla-only set and its byte-identical layout.
fn collect_dxt_brewing(dxt: &mut DxtEntries) {
    let stands = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner());
    let mut entries: Vec<_> = stands
        .iter()
        .filter(|(pos, _)| !pos.0.is_vanilla())
        .collect();
    entries.sort_by_key(|(&(dim, x, y, z), _)| (dim_key(dim), x, y, z));
    for (&(dim, x, y, z), entry) in entries {
        let slots = entry
            .menu
            .slots
            .iter()
            .enumerate()
            .filter(|(_, stack)| stack.1 > 0)
            .map(|(i, stack)| (i as i16, stack.clone()))
            .collect();
        let current =
            item_to_name(entry.menu.slots[3].0).and_then(BrewingIngredient::from_item);
        let remaining = if current == entry.stand.active_ingredient {
            entry.stand.brew_time
        } else {
            0
        };
        dxt.brewing
            .push((dim_key(dim), x, y, z, slots, entry.stand.fuel, remaining));
    }
}

fn append_brewing_footer(out: &mut Vec<u8>) {
    let stands = live_brewing_stands().lock().unwrap_or_else(|e| e.into_inner());
    let mut entries: Vec<_> = stands
        .iter()
        .filter(|(pos, stand)| {
            pos.0.is_vanilla()
                && (stand.stand.fuel > 0
                    || stand.menu.slots.iter().any(|stack| stack.1 > 0))
        })
        .collect();
    if entries.is_empty() {
        return;
    }
    entries.sort_by_key(|(&(dim, x, y, z), _)| (dim.index(), x, y, z));
    let start = out.len();
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (&(dim, x, y, z), entry) in entries {
        out.push(dim.index() as u8);
        for coordinate in [x, y, z] {
            out.extend_from_slice(&coordinate.to_be_bytes());
        }
        let slots = entry
            .menu
            .slots
            .iter()
            .enumerate()
            .filter(|(_, stack)| stack.1 > 0)
            .map(|(i, stack)| (i as i16, stack.clone()))
            .collect();
        write_stack_table(out, &slots);
        out.push(entry.stand.fuel);
        // If the ingredient was swapped since the last tick, the outstanding
        // recipe is already invalid. The loaded entity can start a new one.
        let current = item_to_name(entry.menu.slots[3].0).and_then(BrewingIngredient::from_item);
        let remaining = if current == entry.stand.active_ingredient {
            entry.stand.brew_time
        } else {
            0
        };
        out.extend_from_slice(&remaining.to_be_bytes());
    }
    out.extend_from_slice(&((out.len() - start) as u64).to_be_bytes());
    out.extend_from_slice(BREWING_SAVE_MAGIC);
}

fn read_brewing_footer(
    payload: &[u8],
) -> Result<(&[u8], HashMap<DimBlockPos, LiveBrewingStand>), &'static str> {
    let mut stands = HashMap::new();
    if !payload.ends_with(BREWING_SAVE_MAGIC) {
        return Ok((payload, stands));
    }
    let end = payload
        .len()
        .checked_sub(12)
        .ok_or("truncated brewing footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid brewing footer size")?;
    let start = end.checked_sub(size).ok_or("invalid brewing footer size")?;
    let body = &payload[start..end];
    let mut p = 0;
    let count = read_dim2_count(body, &mut p, 17).map_err(|_| "invalid brewing stand count")?;
    for _ in 0..count {
        let header = body
            .get(p..p + 13)
            .ok_or("truncated brewing stand position")?;
        p += header.len();
        let dim = DimId::from_index(header[0] as usize).ok_or("invalid brewing dimension")?;
        let coordinate = |i| i32::from_be_bytes(header[i..i + 4].try_into().unwrap());
        let key = (dim, coordinate(1), coordinate(5), coordinate(9));
        let slots = read_stack_table(body, &mut p, true);
        let timers = body.get(p..p + 3).ok_or("truncated brewing stand timers")?;
        p += timers.len();
        let mut entry = LiveBrewingStand::default();
        for (index, stack) in slots {
            *entry
                .menu
                .slots
                .get_mut(index as usize)
                .ok_or("invalid brewing slot")? = stack;
        }
        entry.stand.fuel = timers[0];
        entry.stand.brew_time = u16::from_be_bytes(timers[1..3].try_into().unwrap());
        if entry.stand.fuel > BREW_FUEL_PER_POWDER || entry.stand.brew_time > BREW_TIME_TICKS {
            return Err("invalid brewing stand timers");
        }
        if entry.stand.brew_time > 0 {
            entry.stand.active_ingredient =
                item_to_name(entry.menu.slots[3].0).and_then(BrewingIngredient::from_item);
            if entry.stand.active_ingredient.is_none() {
                return Err("brewing stand has a timer without an ingredient");
            }
        }
        if stands.insert(key, entry).is_some() {
            return Err("duplicate brewing stand position");
        }
    }
    if p != body.len() {
        return Err("trailing bytes in brewing footer");
    }
    Ok((&payload[..start], stands))
}

// ---------- sign display metadata footer ----------
//
// CREATIVE-P02-SALVAGE-1 gave signs live dye/glow/ink/wax state
// (`sign_meta()`): both sides' text colour and glow flag plus the block-level
// wax bit. The sign sections — legacy, DIM1 and DXT1 — carry only the four
// text lines, so without this footer a restart silently returns every dyed,
// glowing or waxed sign to vanilla defaults. Written as an SGN1 footer only
// for entries that differ from those defaults, so a world whose signs were
// never dressed keeps the exact pre-record byte layout. It sits between DXT1
// and WBE1 in the file, which puts its peel between `read_wbe_footer` and
// `read_dxt_footer`.
//
// Body: `[schema u8][count u32]` then per entry
// `[dim key][x i32][y i32][z i32][front colour][front glow][back colour]
// [back glow][waxed]`, where a colour is `[len u16][utf8]` restricted to the
// sixteen vanilla dye names and each flag is a single 0/1 byte. Framed
// `[body][body size u64][SGN1]` like the other save-state footers.
const SIGN_META_MAGIC: &[u8; 4] = b"SGN1";
const SIGN_META_SCHEMA: u8 = 1;
const SIGN_META_FOOTER_LEN: usize = 12;
/// The dye names vanilla 1.21.1 lets a sign side carry.
const SIGN_DYE_COLORS: [&str; 16] = [
    "white", "orange", "magenta", "light_blue", "yellow", "lime", "pink", "gray",
    "light_gray", "cyan", "purple", "blue", "brown", "green", "red", "black",
];
/// `(dim key, x, y, z, meta)` — one sign's non-default display state. The
/// dimension rides as its canonical key, like DXT1, so a datapack reorder
/// between restarts cannot move the record.
type SignMetaRecord = (String, i32, i32, i32, SignBlockMeta);

fn sign_side_is_default(side: &SignSideMeta) -> bool {
    side.color == "black" && !side.has_glowing_text
}

fn sign_meta_is_default(meta: &SignBlockMeta) -> bool {
    sign_side_is_default(&meta.front)
        && sign_side_is_default(&meta.back)
        && !meta.is_waxed
}

fn write_sign_side_meta(out: &mut Vec<u8>, side: &SignSideMeta) {
    out.extend_from_slice(&(side.color.len() as u16).to_be_bytes());
    out.extend_from_slice(side.color.as_bytes());
    out.push(u8::from(side.has_glowing_text));
}

fn append_sign_meta_footer(out: &mut Vec<u8>) {
    // Every stored entry is persisted verbatim when it says something the
    // defaults do not: `sign_meta_at`'s lazy sweep is left to runtime reads so
    // a sign whose chunk is not currently loaded cannot lose its dye state to
    // a save-time block probe.
    let mut entries: Vec<SignMetaRecord> = Vec::new();
    for (&(dim, x, y, z), meta) in sign_meta()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        if !sign_meta_is_default(meta) {
            entries.push((dim_key(dim), x, y, z, meta.clone()));
        }
    }
    if entries.is_empty() {
        return;
    }
    entries.sort_by(|a, b| (&a.0, a.1, a.2, a.3).cmp(&(&b.0, b.1, b.2, b.3)));
    let start = out.len();
    out.push(SIGN_META_SCHEMA);
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (key, x, y, z, meta) in &entries {
        write_dxt_key(out, key);
        out.extend_from_slice(&x.to_be_bytes());
        out.extend_from_slice(&y.to_be_bytes());
        out.extend_from_slice(&z.to_be_bytes());
        write_sign_side_meta(out, &meta.front);
        write_sign_side_meta(out, &meta.back);
        out.push(u8::from(meta.is_waxed));
    }
    let size = (out.len() - start) as u64;
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(SIGN_META_MAGIC);
}

fn read_sign_side_meta(body: &[u8], p: &mut usize) -> Result<SignSideMeta, &'static str> {
    let len = u16::from_be_bytes(
        body.get(*p..*p + 2)
            .ok_or("truncated sign colour length")?
            .try_into()
            .unwrap(),
    ) as usize;
    *p += 2;
    let bytes = body.get(*p..*p + len).ok_or("truncated sign colour")?;
    *p += len;
    let color = std::str::from_utf8(bytes)
        .map_err(|_| "invalid sign colour")?
        .to_string();
    if !SIGN_DYE_COLORS.contains(&color.as_str()) {
        return Err("invalid sign colour");
    }
    let has_glowing_text = match body.get(*p) {
        Some(0) => {
            *p += 1;
            false
        }
        Some(1) => {
            *p += 1;
            true
        }
        _ => return Err("invalid sign glow flag"),
    };
    Ok(SignSideMeta {
        color,
        has_glowing_text,
    })
}

fn read_sign_meta_footer(payload: &[u8]) -> Result<(&[u8], Vec<SignMetaRecord>), &'static str> {
    if !payload.ends_with(SIGN_META_MAGIC) {
        return Ok((payload, Vec::new()));
    }
    let end = payload
        .len()
        .checked_sub(SIGN_META_FOOTER_LEN)
        .ok_or("truncated sign meta footer")?;
    let size = usize::try_from(u64::from_be_bytes(
        payload[end..end + 8].try_into().unwrap(),
    ))
    .map_err(|_| "invalid sign meta footer size")?;
    let start = end.checked_sub(size).ok_or("invalid sign meta footer size")?;
    let body = &payload[start..end];
    let mut p = 0usize;
    if body.first() != Some(&SIGN_META_SCHEMA) {
        return Err("unsupported sign meta footer");
    }
    p += 1;
    let count = u32::from_be_bytes(
        body.get(p..p + 4)
            .ok_or("truncated sign meta count")?
            .try_into()
            .unwrap(),
    ) as usize;
    p += 4;
    // The smallest legal entry is an empty dim key (2 length bytes), 12
    // position bytes, two "red" colours at 6 bytes each, and the wax byte:
    // 27 bytes.
    if count > body.len().saturating_sub(p) / 27 {
        return Err("invalid sign meta count");
    }
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let key = read_dxt_key(body, &mut p)?.to_string();
        let pos = body
            .get(p..p + 12)
            .ok_or("truncated sign meta position")?;
        p += 12;
        let x = i32::from_be_bytes(pos[..4].try_into().unwrap());
        let y = i32::from_be_bytes(pos[4..8].try_into().unwrap());
        let z = i32::from_be_bytes(pos[8..12].try_into().unwrap());
        let front = read_sign_side_meta(body, &mut p)?;
        let back = read_sign_side_meta(body, &mut p)?;
        let is_waxed = match body.get(p) {
            Some(0) => {
                p += 1;
                false
            }
            Some(1) => {
                p += 1;
                true
            }
            _ => return Err("invalid sign wax flag"),
        };
        records.push((
            key,
            x,
            y,
            z,
            SignBlockMeta {
                front,
                back,
                is_waxed,
            },
        ));
    }
    if p != body.len() {
        return Err("trailing bytes in sign meta footer");
    }
    Ok((&payload[..start], records))
}

// ---------- W-BE mechanism registry footer ----------
//
// The world-mechanisms registry (`world_block_entity_registry`) owns the nine
// live block-entity families — brewing stand, crafter, campfire, jukebox,
// spawner, lectern, chiseled bookshelf, decorated pot and beacon — keyed by
// dimension and block. Written as a WBE1 footer only when the registry holds
// something, so a world without mechanism entities keeps the exact pre-record
// byte layout (and `the_tick_preserves_the_saved_world_payload_byte_for_byte`
// sees not one extra byte). Framed `[body][body size u64][WBE1]` like the
// other save-state footers; each entry is `[dim key u16+utf8][x][y][z][kind u8]
// [payload len u32][payload]`, where the payload is named NBT for the eight
// `to_nbt` families and a compact binary record for the component-aware live
// brewing stand.
const WBE_MAGIC: &[u8; 4] = b"WBE1";

/// Kind tags in `VanillaBlockEntity` declaration order.
fn wbe_entity_tag(entity: &VanillaBlockEntity) -> u8 {
    match entity {
        VanillaBlockEntity::BrewingStand(_) => 0,
        VanillaBlockEntity::Crafter(_) => 1,
        VanillaBlockEntity::Campfire(_) => 2,
        VanillaBlockEntity::Jukebox(_) => 3,
        VanillaBlockEntity::MobSpawner(_) => 4,
        VanillaBlockEntity::Lectern(_) => 5,
        VanillaBlockEntity::ChiseledBookshelf(_) => 6,
        VanillaBlockEntity::DecoratedPot(_) => 7,
        VanillaBlockEntity::Beacon(_) => 8,
    }
}

// Declaration-order tables for the brewing stand's fieldless enums: encode is
// the `as u8` discriminant, decode indexes back into the same table.
const WBE_POTION_KINDS: [PotionKind; 46] = [
    PotionKind::Water,
    PotionKind::Mundane,
    PotionKind::Thick,
    PotionKind::Awkward,
    PotionKind::NightVision,
    PotionKind::LongNightVision,
    PotionKind::Invisibility,
    PotionKind::LongInvisibility,
    PotionKind::Leaping,
    PotionKind::LongLeaping,
    PotionKind::StrongLeaping,
    PotionKind::FireResistance,
    PotionKind::LongFireResistance,
    PotionKind::Swiftness,
    PotionKind::LongSwiftness,
    PotionKind::StrongSwiftness,
    PotionKind::Slowness,
    PotionKind::LongSlowness,
    PotionKind::StrongSlowness,
    PotionKind::TurtleMaster,
    PotionKind::LongTurtleMaster,
    PotionKind::StrongTurtleMaster,
    PotionKind::WaterBreathing,
    PotionKind::LongWaterBreathing,
    PotionKind::Healing,
    PotionKind::StrongHealing,
    PotionKind::Harming,
    PotionKind::StrongHarming,
    PotionKind::Poison,
    PotionKind::LongPoison,
    PotionKind::StrongPoison,
    PotionKind::Regeneration,
    PotionKind::LongRegeneration,
    PotionKind::StrongRegeneration,
    PotionKind::Strength,
    PotionKind::LongStrength,
    PotionKind::StrongStrength,
    PotionKind::Weakness,
    PotionKind::LongWeakness,
    PotionKind::Luck,
    PotionKind::SlowFalling,
    PotionKind::LongSlowFalling,
    PotionKind::WindCharged,
    PotionKind::Oozing,
    PotionKind::Infested,
    PotionKind::Weaving,
];
const WBE_POTION_FORMS: [PotionForm; 3] = [
    PotionForm::Drinkable,
    PotionForm::Splash,
    PotionForm::Lingering,
];
const WBE_INGREDIENTS: [BrewingIngredient; 22] = [
    BrewingIngredient::NetherWart,
    BrewingIngredient::FermentedSpiderEye,
    BrewingIngredient::GoldenCarrot,
    BrewingIngredient::RabbitFoot,
    BrewingIngredient::MagmaCream,
    BrewingIngredient::Sugar,
    BrewingIngredient::TurtleHelmet,
    BrewingIngredient::Pufferfish,
    BrewingIngredient::GlisteringMelonSlice,
    BrewingIngredient::SpiderEye,
    BrewingIngredient::GhastTear,
    BrewingIngredient::BlazePowder,
    BrewingIngredient::PhantomMembrane,
    BrewingIngredient::BreezeRod,
    BrewingIngredient::SlimeBlock,
    BrewingIngredient::Stone,
    BrewingIngredient::Cobweb,
    BrewingIngredient::Redstone,
    BrewingIngredient::GlowstoneDust,
    BrewingIngredient::Gunpowder,
    BrewingIngredient::DragonBreath,
    BrewingIngredient::GlassBottle,
];

/// The live brewing stand's binary payload: `[blaze_powder u8][fuel u8]
/// [brew_time u16][ingredient item u8 = 0xFF none][ingredient count u8 when set]
/// [active_ingredient u8 = 0xFF none]` then three bottles, each `[kind u8 =
/// 0xFF none][form u8 when set]`.
fn wbe_write_brewing(stand: &BrewingStand, out: &mut Vec<u8>) {
    out.push(stand.blaze_powder);
    out.push(stand.fuel);
    out.extend_from_slice(&stand.brew_time.to_be_bytes());
    match stand.ingredient {
        Some((item, count)) => {
            out.push(item as u8);
            out.push(count);
        }
        None => out.push(0xFF),
    }
    out.push(stand.active_ingredient.map_or(0xFF, |item| item as u8));
    for bottle in &stand.bottles {
        match bottle {
            Some(potion) => {
                out.push(potion.kind as u8);
                out.push(potion.form as u8);
            }
            None => out.push(0xFF),
        }
    }
}

fn wbe_read_brewing(payload: &[u8]) -> Option<BrewingStand> {
    let mut input = SaveInput(payload);
    let blaze_powder = input.byte().ok()?;
    let fuel = input.byte().ok()?;
    let brew_time = u16::from_be_bytes(input.take().ok()?);
    let ingredient = match input.byte().ok()? {
        0xFF => None,
        item => Some((
            WBE_INGREDIENTS.get(usize::from(item)).copied()?,
            input.byte().ok()?,
        )),
    };
    let active_ingredient = match input.byte().ok()? {
        0xFF => None,
        item => Some(WBE_INGREDIENTS.get(usize::from(item)).copied()?),
    };
    let mut bottles = [None, None, None];
    for bottle in &mut bottles {
        match input.byte().ok()? {
            0xFF => {}
            kind => {
                let kind = WBE_POTION_KINDS.get(usize::from(kind)).copied()?;
                let form = WBE_POTION_FORMS.get(usize::from(input.byte().ok()?)).copied()?;
                *bottle = Some(BrewingPotion { kind, form });
            }
        }
    }
    if !input.0.is_empty() {
        return None;
    }
    Some(BrewingStand {
        bottles,
        ingredient,
        blaze_powder,
        fuel,
        brew_time,
        active_ingredient,
    })
}

/// One registry entry's opaque payload: named NBT for the eight `to_nbt`
/// families, the binary record above for the live brewing stand.
fn wbe_entity_payload(entity: &VanillaBlockEntity) -> Option<Vec<u8>> {
    let tag = match entity {
        VanillaBlockEntity::BrewingStand(stand) => {
            let mut payload = Vec::new();
            wbe_write_brewing(stand, &mut payload);
            return Some(payload);
        }
        VanillaBlockEntity::Crafter(be) => be.to_nbt(),
        VanillaBlockEntity::Campfire(be) => be.to_nbt(),
        VanillaBlockEntity::Jukebox(be) => be.to_nbt(),
        VanillaBlockEntity::MobSpawner(be) => be.to_nbt(),
        VanillaBlockEntity::Lectern(be) => be.to_nbt(),
        VanillaBlockEntity::ChiseledBookshelf(be) => be.to_nbt(),
        VanillaBlockEntity::DecoratedPot(be) => be.to_nbt(),
        VanillaBlockEntity::Beacon(be) => be.to_nbt(),
    };
    crate::nbt::write_named(&crate::nbt::NbtString::text(""), &tag).ok()
}

fn wbe_entity_from_payload(
    kind: u8,
    payload: &[u8],
) -> Result<VanillaBlockEntity, &'static str> {
    if kind == 0 {
        return wbe_read_brewing(payload)
            .map(VanillaBlockEntity::BrewingStand)
            .ok_or("invalid W-BE brewing stand record");
    }
    let (name, tag) = crate::nbt::read_named(payload)?;
    if !name.0.is_empty() || !matches!(tag, crate::nbt::Tag::Compound(_)) {
        return Err("W-BE record must be an unnamed compound");
    }
    match kind {
        1 => CrafterBlockEntity::from_nbt(&tag).map(VanillaBlockEntity::Crafter),
        2 => CampfireBlockEntity::from_nbt(&tag).map(VanillaBlockEntity::Campfire),
        3 => JukeboxBlockEntity::from_nbt(&tag).map(VanillaBlockEntity::Jukebox),
        4 => MobSpawnerBlockEntity::from_nbt(&tag).map(VanillaBlockEntity::MobSpawner),
        5 => LecternBlockEntity::from_nbt(&tag).map(VanillaBlockEntity::Lectern),
        6 => ChiseledBookshelfBlockEntity::from_nbt(&tag)
            .map(VanillaBlockEntity::ChiseledBookshelf),
        7 => DecoratedPotBlockEntity::from_nbt(&tag).map(VanillaBlockEntity::DecoratedPot),
        8 => BeaconBlockEntity::from_nbt(&tag).map(VanillaBlockEntity::Beacon),
        _ => return Err("unknown W-BE entity kind"),
    }
    .ok_or("invalid W-BE entity payload")
}

/// `(dim block pos, entity kind, NBT blob)` — one saved block entity outside
/// the Overworld.
type WbeFooterEntry = ((DimId, i32, i32, i32), u8, Vec<u8>);

fn append_wbe_footer(out: &mut Vec<u8>) {
    let mut entries: Vec<WbeFooterEntry> = Vec::new();
    {
        let registry = world_block_entity_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (&key, entity) in registry.iter() {
            if key.0.dimension_id().is_empty() {
                continue;
            }
            let Some(payload) = wbe_entity_payload(entity) else {
                continue;
            };
            entries.push((key, wbe_entity_tag(entity), payload));
        }
    }
    if entries.is_empty() {
        return;
    }
    entries.sort_by_key(|(key, kind, _)| (dim_key(key.0), key.1, key.2, key.3, *kind));
    let start = out.len();
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for ((dim, x, y, z), kind, payload) in entries {
        let key = dim_key(dim);
        out.extend_from_slice(&(key.len() as u16).to_be_bytes());
        out.extend_from_slice(key.as_bytes());
        save_numbers!(out, x, y, z);
        out.push(kind);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&payload);
    }
    out.extend_from_slice(&((out.len() - start) as u64).to_be_bytes());
    out.extend_from_slice(WBE_MAGIC);
}

/// The block entities a WBE1 body carries back: position to the live record.
type SavedBlockEntities = Vec<(DimBlockPos, VanillaBlockEntity)>;

fn read_wbe_footer(
    payload: &[u8],
) -> Result<(&[u8], SavedBlockEntities), &'static str> {
    let (payload, body) = split_save_footer(payload, WBE_MAGIC)?;
    let Some(body) = body else {
        return Ok((payload, Vec::new()));
    };
    let mut input = SaveInput(body);
    let count = u32::from_be_bytes(input.take()?) as usize;
    // One entry is at least 2 + 12 + 1 + 4 bytes; a larger count is corrupt.
    if count > input.0.len() / 19 {
        return Err("invalid W-BE entity count");
    }
    let mut entries = Vec::with_capacity(count);
    let mut positions = HashSet::new();
    for _ in 0..count {
        let key_len = usize::from(u16::from_be_bytes(input.take()?));
        let key = std::str::from_utf8(input.bytes(key_len)?)
            .map_err(|_| "invalid W-BE dimension key")?
            .to_owned();
        let (x, y, z) = (
            i32::from_be_bytes(input.take()?),
            i32::from_be_bytes(input.take()?),
            i32::from_be_bytes(input.take()?),
        );
        let kind = input.byte()?;
        let len = u32::from_be_bytes(input.take()?) as usize;
        let entity = wbe_entity_from_payload(kind, input.bytes(len)?)?;
        // A dimension a dropped datapack took with it keeps nothing here, the
        // same convention DXT1 records follow.
        let Some(dim) = dxt_dim(&key) else {
            continue;
        };
        if !positions.insert((dim, x, y, z)) {
            return Err("duplicate W-BE entity position");
        }
        entries.push(((dim, x, y, z), entity));
    }
    if !input.0.is_empty() {
        return Err("trailing bytes in W-BE footer");
    }
    Ok((payload, entries))
}

/// Install WBE1 records as the live mechanism registry: the save is
/// authoritative, so whatever was registered is replaced wholesale.
fn restore_wbe_entries(entries: Vec<(DimBlockPos, VanillaBlockEntity)>) {
    let mut registry = world_block_entity_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    registry.clear();
    for (key, entity) in entries {
        registry.insert(key, entity);
    }
}

// ---------- mod-bridge trailers (MOF1 / AER1) ----------
//
// Optional trailing records for the mod bridges: `MOF1` carries the
// Create/mod-offload state (placed Create blocks + live contraptions) and
// `AER1` the aeronautics fleet plus discovered dimension environments. Each
// body is `[u8 schema][u32 inner len][u32 crc32(inner)][inner]`, framed
// `[body][u64 body size][magic]` like every other optional trailer — and each
// is emitted only while its bridge holds state, so a vanilla save keeps the
// exact pre-record tail byte-for-byte.

const MOD_OFFLOAD_TRAILER_MAGIC: &[u8; 4] = b"MOF1";
const AERONAUTICS_TRAILER_MAGIC: &[u8; 4] = b"AER1";
/// Sanity bound on a mod trailer body; a real fleet/factory is nowhere near.
const MOD_TRAILER_MAX_BODY: usize = 1 << 26;
const MOD_TRAILER_SCHEMA: u8 = 1;

/// Emit the optional `MOF1`/`AER1` trailers for any bridge holding state.
/// With the `create-offload` feature off — or both bridges empty — this
/// appends zero bytes.
fn append_mod_bridge_trailers(out: &mut Vec<u8>) {
    #[cfg(feature = "create-offload")]
    {
        if mod_offload_has_state() {
            append_mod_bridge_trailer(out, MOD_OFFLOAD_TRAILER_MAGIC, &mod_offload_save_bytes());
        }
        if aeronautics_bridge_has_state() {
            append_mod_bridge_trailer(
                out,
                AERONAUTICS_TRAILER_MAGIC,
                &aeronautics_state_save_bytes(),
            );
        }
    }
    let _ = out;
}

/// Frame `inner` as one mod trailer: schema + declared length + CRC32 of the
/// inner bytes, then the `[body][u64 body size][magic]` wrapper every optional
/// footer uses.
#[allow(dead_code)]
fn append_mod_bridge_trailer(out: &mut Vec<u8>, magic: &[u8; 4], inner: &[u8]) {
    let start = out.len();
    out.push(MOD_TRAILER_SCHEMA);
    save_numbers!(out, inner.len() as u32, persist_crc32(inner));
    out.extend_from_slice(inner);
    save_numbers!(out, (out.len() - start) as u64);
    out.extend_from_slice(magic);
}

/// Peel one optional mod trailer: returns the payload minus the trailer plus
/// the CRC-validated inner bytes, `None` when the trailer is absent. Bad
/// framing, a mismatched length, or a checksum failure all fail closed.
fn read_mod_bridge_trailer<'a>(
    payload: &'a [u8],
    magic: &[u8; 4],
) -> Result<(&'a [u8], Option<&'a [u8]>), &'static str> {
    let (payload, body) = split_save_footer(payload, magic)?;
    let Some(body) = body else {
        return Ok((payload, None));
    };
    if body.len() > MOD_TRAILER_MAX_BODY {
        return Err("mod bridge trailer out of range");
    }
    let mut input = SaveInput(body);
    if input.byte()? != MOD_TRAILER_SCHEMA {
        return Err("unsupported mod bridge trailer");
    }
    let inner_len = usize::try_from(u32::from_be_bytes(input.take()?))
        .map_err(|_| "invalid mod bridge trailer")?;
    let crc = u32::from_be_bytes(input.take()?);
    let inner = input.bytes(inner_len)?;
    if persist_crc32(inner) != crc {
        return Err("mod bridge trailer checksum mismatch");
    }
    if !input.0.is_empty() {
        return Err("trailing bytes in mod bridge trailer");
    }
    Ok((payload, Some(inner)))
}

/// Restore the mod-bridge trailers after the core world decoded cleanly.
/// Each bridge codec itself is all-or-nothing; a save carrying mod state on a
/// build that cannot restore it fails closed rather than silently dropping
/// the contraptions and airships it stored.
#[cfg(feature = "create-offload")]
fn apply_mod_bridge_trailers(
    mof1: Option<&[u8]>,
    aer1: Option<&[u8]>,
) -> Result<(), &'static str> {
    if mof1.is_some_and(|inner| !mod_offload_load_bytes(inner)) {
        return Err("corrupt mod offload trailer");
    }
    if aer1.is_some_and(|inner| !aeronautics_state_load_bytes(inner)) {
        return Err("corrupt aeronautics trailer");
    }
    Ok(())
}

/// Default builds carry no mod bridge: a `MOF1`/`AER1` trailer must be an
/// error, not a silent drop of the state it stored.
#[cfg(not(feature = "create-offload"))]
fn apply_mod_bridge_trailers(
    mof1: Option<&[u8]>,
    aer1: Option<&[u8]>,
) -> Result<(), &'static str> {
    if mof1.is_some() || aer1.is_some() {
        return Err("world snapshot carries mod bridge state this build cannot load");
    }
    Ok(())
}

// Live block-entity snapshots (#216): beacon, campfire, lectern, chiseled
// bookshelf, decorated pot, crafter, jukebox, banner, skull, brushable block,
// trial spawner and vault. Each entry carries its position, the block state it
// was snapshotted against and the vanilla named-NBT payload, framed
// `[body][body size u64][BEN1]` like the other save-state footers. It is written
// between the legacy region and SVP1 so a save with no block entities keeps
// every legacy byte — and every other footer's position — exactly where it was.
const BLOCK_ENTITIES_MAGIC: &[u8; 4] = b"BEN1";

fn append_block_entities_footer(out: &mut Vec<u8>, dxt: &mut DxtEntries) {
    let Some(reg) = breg() else { return };
    // Pick up blocks placed since the last tick's scan so entities nobody has
    // interacted with persist too.
    live_block_entities().lock().unwrap_or_else(|e| e.into_inner()).discover(reg);
    let keys: Vec<DimBlockPos> = live_block_entities()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .keys()
        .copied()
        .collect();
    let mut entries = Vec::new();
    for key in keys {
        if let Some((state, tag)) = live_block_entity_snapshot(key) {
            if key.0.is_vanilla() {
                entries.push((key, state, tag));
            } else {
                dxt.block_entities
                    .push((dim_key(key.0), key.1, key.2, key.3, state, tag));
            }
        }
    }
    if entries.is_empty() {
        return;
    }
    entries.sort_by_key(|&((dim, x, y, z), _, _)| (dim.index(), x, y, z));
    let start = out.len();
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for ((dim, x, y, z), state, tag) in entries {
        out.push(dim.index() as u8);
        save_numbers!(out, x, y, z, state);
        let nbt = nbt::write_named(&nbt::NbtString::text(""), &tag)
            .expect("block entity NBT must encode");
        out.extend_from_slice(&(nbt.len() as u32).to_be_bytes());
        out.extend_from_slice(&nbt);
    }
    out.extend_from_slice(&((out.len() - start) as u64).to_be_bytes());
    out.extend_from_slice(BLOCK_ENTITIES_MAGIC);
}

fn read_block_entities_body(
    body: &[u8],
) -> Result<Vec<(DimBlockPos, u32, nbt::Tag)>, &'static str> {
    let mut entries = Vec::new();
    let mut positions = HashSet::new();
    let mut p = 0;
    let count = read_dim2_count(body, &mut p, 21).map_err(|_| "invalid block entity count")?;
    for _ in 0..count {
        let header = body.get(p..p + 17).ok_or("truncated block entity")?;
        p += header.len();
        let dim =
            DimId::from_index(header[0] as usize).ok_or("invalid block entity dimension")?;
        let coordinate =
            |i| i32::from_be_bytes(header[i..i + 4].try_into().unwrap());
        let key @ (_, x, y, z) = (dim, coordinate(1), coordinate(5), coordinate(9));
        let state = u32::from_be_bytes(header[13..17].try_into().unwrap());
        let len = u32::from_be_bytes(
            body.get(p..p + 4)
                .ok_or("truncated block entity NBT length")?
                .try_into()
                .unwrap(),
        ) as usize;
        p += 4;
        let data = body.get(p..p + len).ok_or("truncated block entity NBT")?;
        p += len;
        let (name, tag) = nbt::read_named(data)?;
        if !name.0.is_empty() || !matches!(tag, nbt::Tag::Compound(_)) {
            return Err("block entity root must be an unnamed compound");
        }
        if block_entity_str_field(&tag, "id").is_none() {
            return Err("block entity is missing its type id");
        }
        if block_entity_int(&tag, "x") != x
            || block_entity_int(&tag, "y") != y
            || block_entity_int(&tag, "z") != z
        {
            return Err("block entity position mismatch");
        }
        if !positions.insert(key) {
            return Err("duplicate block entity position");
        }
        entries.push((key, state, tag));
    }
    if p != body.len() {
        return Err("trailing bytes in block entity footer");
    }
    Ok(entries)
}

// The saved block state is authoritative: the snapshot was taken atomically with
// the world edits, and a non-Overworld dimension's overlay file lands after the
// main payload has already run. The store's discovery keeps reconciling entries
// against the live world, so a block that does not come back drops the record on
// the next scan of its chunk.
fn restore_saved_block_entities(entries: Option<Vec<(DimBlockPos, u32, nbt::Tag)>>) {
    let mut store = live_block_entities().lock().unwrap_or_else(|e| e.into_inner());
    store.entries.clear();
    let (Some(reg), Some(entries)) = (breg(), entries) else {
        return;
    };
    for (key, state, tag) in entries {
        let Some(id) = live_block_entity_id(reg.name_of(state)) else {
            continue;
        };
        if block_entity_str_field(&tag, "id").as_deref() != Some(id) {
            continue;
        }
        let Some(block) = reg.block_id_of(state) else {
            continue;
        };
        store
            .entries
            .insert(key, (block, LiveBlockEntity::load(reg, state, id, &tag)));
    }
}

#[cfg(test)]
mod world_seed_persistence_tests {
    use super::*;

    #[test]
    fn world_seed_footer_round_trips_and_preserves_legacy_payloads() {
        let legacy = b"legacy save";
        assert_eq!(read_world_seed_footer(legacy), Ok((legacy.as_slice(), None)));

        let mut payload = legacy.to_vec();
        append_world_seed_footer(&mut payload, u64::MAX);
        assert_eq!(
            read_world_seed_footer(&payload),
            Ok((legacy.as_slice(), Some(u64::MAX)))
        );
    }

    #[test]
    fn saved_seed_is_default_and_mismatched_override_is_rejected() {
        assert_eq!(resolve_world_seed(None, Some(123)), Ok(123));
        assert_eq!(resolve_world_seed(Some(123), Some(123)), Ok(123));
        assert_eq!(resolve_world_seed(None, None), Ok(0));
        assert_eq!(
            resolve_world_seed(Some(456), Some(123)),
            Err("MEGU_SEED does not match the saved world seed")
        );
    }
}

#[cfg(test)]
mod save_integrity_tests {
    use super::*;
    #[test]
    fn wrapped_world_roundtrip_and_legacy_acceptance() {
        let legacy = vec![0u8, b'M', b'E', b'T', b'3', 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(unwrap_world_payload(legacy.clone()), Ok(legacy.clone()));
        assert_eq!(
            unwrap_world_payload(wrap_world_payload(&legacy)),
            Ok(legacy)
        );
    }
    #[test]
    fn truncated_and_damaged_new_saves_are_rejected() {
        let original = wrap_world_payload(b"world-data");
        for n in 0..original.len() {
            let short = original[..n].to_vec();
            if n >= 3 {
                assert!(unwrap_world_payload(short).is_err(), "truncated to {n}");
            }
        }
        let mut corrupt = original;
        *corrupt.last_mut().unwrap() ^= 1;
        assert_eq!(unwrap_world_payload(corrupt), Err("save checksum mismatch"));
    }
}

#[cfg(test)]
mod save_backup_tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spine-save-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saves_keep_three_backup_generations_and_no_tmp_left() {
        let dir = scratch("gens");
        let world = dir.join("world.bin");
        // interval 0: every save rotates, so the generations are easy to read
        for n in 1..=5u8 {
            write_world_file(&world, &[n], Duration::ZERO).unwrap();
        }
        let read = |name: &str| std::fs::read(dir.join(name)).unwrap();
        assert_eq!(read("world.bin"), [5]);
        assert_eq!(read("world.bin.bak"), [4]);
        assert_eq!(read("world.bin.bak.1"), [3]);
        assert_eq!(read("world.bin.bak.2"), [2]);
        assert!(!dir.join("world.bin.bak.3").exists());
        assert!(!dir.join("world.bin.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn older_generations_do_not_advance_within_the_interval() {
        let dir = scratch("interval");
        let world = dir.join("world.bin");
        for n in 1..=5u8 {
            write_world_file(&world, &[n], Duration::from_secs(3600)).unwrap();
        }
        let read = |name: &str| std::fs::read(dir.join(name)).unwrap();
        assert_eq!(read("world.bin"), [5]);
        assert_eq!(
            read("world.bin.bak"),
            [4],
            ".bak is always the previous save"
        );
        // bak.1 was created on the second save and is far younger than an hour
        assert_eq!(read("world.bin.bak.1"), [1]);
        assert!(!dir.join("world.bin.bak.2").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_save_is_reported_with_its_path_and_keeps_the_old_world() {
        let dir = scratch("fail");
        let world = dir.join("world.bin");
        write_world_file(&world, b"good", Duration::ZERO).unwrap();
        // a directory squatting on the tmp path makes the tmp write fail
        std::fs::create_dir(dir.join("world.bin.tmp")).unwrap();
        let err = write_world_file(&world, b"new", Duration::ZERO).unwrap_err();
        assert!(err.contains("world.bin.tmp"), "{err}");
        assert_eq!(std::fs::read(&world).unwrap(), b"good");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod save_v2_version_tests {
    use super::*;

    /// Run `run` with `MEGU_TARGET_VERSION` / `MEGU_MC_VERSION` set to
    /// the given values, then put back whatever was there before — even on a
    /// panic, so a failed assertion cannot leak a target into the next test.
    fn with_target_env<R>(
        target: Option<&str>,
        legacy: Option<&str>,
        run: impl FnOnce() -> R,
    ) -> R {
        struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl std::ops::Drop for Restore {
            fn drop(&mut self) {
                for (name, value) in &self.0 {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
        let vars = ["MEGU_TARGET_VERSION", "MEGU_MC_VERSION"];
        let _restore = Restore(
            vars.iter()
                .map(|name| (*name, std::env::var_os(name)))
                .collect(),
        );
        for (name, value) in [
            ("MEGU_TARGET_VERSION", target),
            ("MEGU_MC_VERSION", legacy),
        ] {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        run()
    }

    #[test]
    fn save_v2_version_header_round_trips_for_known_releases() {
        for header in [
            SaveVersionHeader::v1_21_1(),
            SaveVersionHeader::for_mc_version("1.21.4").unwrap(),
            SaveVersionHeader::for_mc_version("26.3").unwrap(),
        ] {
            let mut payload = b"save-body".to_vec();
            append_save_version_footer(&mut payload, &header);
            append_world_seed_footer(&mut payload, 42);
            let got = read_save_version_header(&payload).unwrap().unwrap();
            assert_eq!(got.mc_version, header.mc_version);
            assert_eq!(got.data_version, header.data_version);
            assert_eq!(got.protocol_version, header.protocol_version);
            assert!(!got.inferred_legacy_header);
            // The seed strip hides VER1 completely: the caller sees the frame
            // a pre-header save would have ended with.
            let (rest, seed) = read_world_seed_footer(&payload).unwrap();
            assert_eq!(seed, Some(42));
            assert_eq!(rest, b"save-body");
        }
        // Known milestones map to vanilla's version pair.
        let h = SaveVersionHeader::for_mc_version("1.21.4").unwrap();
        assert_eq!((h.data_version, h.protocol_version), (4189, 769));
        // Both first-class targets are table entries.
        let h = SaveVersionHeader::for_mc_version("1.21.1").unwrap();
        assert_eq!((h.data_version, h.protocol_version), (3955, 767));
        let h = SaveVersionHeader::for_mc_version("26.3").unwrap();
        assert_eq!((h.data_version, h.protocol_version), (5023, 777));
        for target in SAVE_TARGET_VERSIONS {
            assert!(
                SaveVersionHeader::for_mc_version(target).is_some(),
                "{target} must have a version-table entry"
            );
        }
        // A name the table does not know fails closed instead of guessing a
        // DataVersion pair it did not write.
        assert_eq!(SaveVersionHeader::for_mc_version("1.21.5-custom"), None);
        assert_eq!(SaveVersionHeader::for_mc_version("26.4"), None);
        assert_eq!(SaveVersionHeader::for_mc_version(""), None);
    }

    #[test]
    fn save_v2_missing_version_header_infers_legacy_1211() {
        let mut payload = b"save-body".to_vec();
        append_world_seed_footer(&mut payload, 7);
        assert_eq!(read_save_version_header(&payload).unwrap(), None);
        let header = SaveVersionHeader::legacy();
        assert!(header.inferred_legacy_header);
        assert_eq!(header.mc_version, "1.21.1");
        assert_eq!((header.data_version, header.protocol_version), (3955, 767));
    }

    #[test]
    fn save_v2_version_header_survives_seed_only_strip() {
        // A payload that already lost its seed trailer still yields the record.
        let mut payload = b"save-body".to_vec();
        append_save_version_footer(&mut payload, &SaveVersionHeader::v1_21_1());
        let got = read_save_version_header(&payload).unwrap().unwrap();
        assert_eq!(got.mc_version, "1.21.1");
        assert!(!got.inferred_legacy_header);
    }

    #[test]
    fn save_v2_version_header_fails_closed_on_corruption() {
        // An honest non-VER1 tail is simply "no header", never a misparse.
        let mut payload = b"save-body".to_vec();
        payload.extend_from_slice(b"XXXX");
        assert_eq!(read_save_version_header(&payload).unwrap(), None);
        // A VER1 trailer whose declared body cannot exist is an error.
        let mut bad = b"body".to_vec();
        bad.extend_from_slice(&999u64.to_be_bytes());
        bad.extend_from_slice(SAVE_VERSION_MAGIC);
        assert!(read_save_version_header(&bad).is_err());
        // An unknown footer schema version is an error, not a silent skip.
        let mut bad = b"body".to_vec();
        bad.push(9);
        let size = 1u64;
        bad.extend_from_slice(&size.to_be_bytes());
        bad.extend_from_slice(SAVE_VERSION_MAGIC);
        assert!(read_save_version_header(&bad).is_err());
        // Trailing junk inside the body is an error too.
        let mut bad = b"body".to_vec();
        bad.extend_from_slice(&[1, 0, 0]);
        let size = 3u64;
        bad.extend_from_slice(&size.to_be_bytes());
        bad.extend_from_slice(SAVE_VERSION_MAGIC);
        assert!(read_save_version_header(&bad).is_err());
    }

    #[test]
    fn save_v2_classify_version_marks_exact_older_and_newer() {
        assert_eq!(
            classify_save_v2_version(&SaveVersionHeader::v1_21_1(), SAVE_DATA_VERSION_RUNNING),
            SaveVersionCompat::Exact
        );
        let older = SaveVersionHeader::for_mc_version("1.20.6").unwrap();
        assert_eq!(
            classify_save_v2_version(&older, SAVE_DATA_VERSION_RUNNING),
            SaveVersionCompat::OlderUpgradeable {
                delta: SAVE_DATA_VERSION_RUNNING - 3839
            }
        );
        let newer = SaveVersionHeader {
            mc_version: "9.9.9".to_owned(),
            data_version: 9999,
            protocol_version: 9999,
            inferred_legacy_header: false,
        };
        assert_eq!(
            classify_save_v2_version(&newer, SAVE_DATA_VERSION_RUNNING),
            SaveVersionCompat::NewerUnknown {
                file_dv: 9999,
                running_dv: SAVE_DATA_VERSION_RUNNING
            }
        );
    }

    #[test]
    fn save_v2_target_env_selects_the_written_header() {
        // `with_target_env` mutates the same process-global vars the guarded
        // save fixtures read through `save_world_into` — serialize them all.
        let _guard = world_guard();
        // `MEGU_TARGET_VERSION` is the canonical selector.
        with_target_env(Some("26.3"), None, || {
            let header = save_version_header().unwrap();
            assert_eq!(
                (
                    header.mc_version.as_str(),
                    header.data_version,
                    header.protocol_version
                ),
                ("26.3", 5023, 777)
            );
        });
        // The legacy `MEGU_MC_VERSION` still selects a target when the
        // canonical variable is unset, and loses to it otherwise.
        with_target_env(None, Some("26.3"), || {
            assert_eq!(save_version_header().unwrap().mc_version, "26.3");
        });
        with_target_env(Some("26.3"), Some("1.21.1"), || {
            assert_eq!(save_version_header().unwrap().mc_version, "26.3");
        });
        // Native or unset: nothing is emitted, the loader infers instead.
        with_target_env(None, None, || assert_eq!(save_version_header(), None));
        with_target_env(Some("1.21.1"), None, || {
            assert_eq!(save_version_header(), None);
        });
    }

    #[test]
    fn save_v2_unknown_target_env_fails_closed() {
        // Same env mutation as `save_v2_target_env_selects_the_written_header`.
        let _guard = world_guard();
        // An unrecognized name can never steer the written schema: the save
        // keeps the native 1.21.1 layout and emits no VER1 claim.
        for bogus in ["bogus", "1.21.4", "26.4"] {
            with_target_env(Some(bogus), None, || {
                assert_eq!(running_save_version(), SaveVersionHeader::v1_21_1());
                assert_eq!(save_version_header(), None);
            });
        }
        // An invalid canonical variable falls through to a valid legacy one.
        with_target_env(Some("bogus"), Some("26.3"), || {
            assert_eq!(running_save_version().mc_version, "26.3");
        });
        // An empty variable counts as unset.
        with_target_env(Some("  "), None, || {
            assert_eq!(running_save_version(), SaveVersionHeader::v1_21_1());
        });
    }

    #[test]
    fn save_v2_native_save_has_no_header_and_infers_legacy_1211() {
        let _guard = world_guard();
        with_target_env(None, None, || {
            let mut payload = Vec::new();
            save_world_into(&mut payload);
            assert!(payload.ends_with(WORLD_SEED_MAGIC));
            // The native write emits no VER1 record, so the frame is
            // byte-for-byte the pre-header layout and the loader infers the
            // 1.21.1 defaults.
            assert_eq!(read_save_version_header(&payload).unwrap(), None);
            load_world_from(&payload).unwrap();
            let loaded = loaded_save_version_header().unwrap();
            assert!(loaded.inferred_legacy_header);
            assert_eq!(loaded.mc_version, "1.21.1");
            assert_eq!(loaded.data_version, SAVE_DATA_VERSION_RUNNING);
        });
    }

    #[test]
    fn save_v2_263_save_round_trips_under_263_target() {
        let _guard = world_guard();
        with_target_env(Some("26.3"), None, || {
            let mut payload = Vec::new();
            save_world_into(&mut payload);
            assert!(payload.ends_with(WORLD_SEED_MAGIC));
            // A 26.3 save must carry VER1 — a tagless frame would be read back
            // as the 1.21.1 legacy form.
            let header = read_save_version_header(&payload).unwrap().unwrap();
            assert_eq!(
                (
                    header.mc_version.as_str(),
                    header.data_version,
                    header.protocol_version
                ),
                ("26.3", 5023, 777)
            );
            load_world_from(&payload).unwrap();
            let loaded = loaded_save_version_header().unwrap();
            assert!(!loaded.inferred_legacy_header);
            assert_eq!(loaded.mc_version, "26.3");
            assert_eq!(loaded.data_version, 5023);
        });
    }

    #[test]
    fn save_v2_foreign_version_header_is_rejected_with_migrate_hint() {
        let _guard = world_guard();
        // A snapshot whose DataVersion does not match the running target is
        // never parsed under the wrong schema; the error names the tool that
        // converts it. Covered: newer (26.3 on a 1.21.1 build), an intermediate
        // milestone, and an older release.
        let mut native = Vec::new();
        with_target_env(None, None, || save_world_into(&mut native));
        let (stripped, seed) = strip_world_seed_footer(&native).unwrap();
        for header in [
            SaveVersionHeader::for_mc_version("26.3").unwrap(),
            SaveVersionHeader::for_mc_version("1.21.4").unwrap(),
            SaveVersionHeader::for_mc_version("1.20.6").unwrap(),
        ] {
            let mut foreign = stripped.to_vec();
            append_save_version_footer(&mut foreign, &header);
            append_world_seed_footer(&mut foreign, seed.unwrap());
            with_target_env(None, None, || {
                let err = load_world_from(&foreign).unwrap_err();
                assert!(
                    err.contains("megu-world-migrate"),
                    "{err} should direct to the migration tool"
                );
            });
        }
    }

    #[test]
    fn save_v2_cross_version_mismatch_is_rejected_both_ways() {
        let _guard = world_guard();
        // A native (untagged) 1.21.1 save loads under the default target but
        // is a cross-version mismatch under a 26.3 target.
        let native = with_target_env(None, None, || {
            let mut payload = Vec::new();
            save_world_into(&mut payload);
            load_world_from(&payload).unwrap();
            payload
        });
        with_target_env(Some("26.3"), None, || {
            let err = load_world_from(&native).unwrap_err();
            assert!(
                err.contains("megu-world-migrate"),
                "{err} should direct to the migration tool"
            );
        });
        // And a VER1-tagged 1.21.1 save is equally foreign to a 26.3 target:
        // the legacy inference and an explicit tag land on the same answer.
        let (stripped, seed) = strip_world_seed_footer(&native).unwrap();
        let mut tagged = stripped.to_vec();
        append_save_version_footer(&mut tagged, &SaveVersionHeader::v1_21_1());
        append_world_seed_footer(&mut tagged, seed.unwrap());
        with_target_env(Some("26.3"), None, || {
            let err = load_world_from(&tagged).unwrap_err();
            assert!(
                err.contains("megu-world-migrate"),
                "{err} should direct to the migration tool"
            );
        });
    }

    #[test]
    fn save_v2_future_data_version_is_rejected() {
        let _guard = world_guard();
        // A DataVersion past anything the table knows can never pass for a
        // native save, even when a future 26.3 target runs the check.
        let mut payload = Vec::new();
        with_target_env(None, None, || save_world_into(&mut payload));
        let (stripped, seed) = strip_world_seed_footer(&payload).unwrap();
        let mut future = stripped.to_vec();
        append_save_version_footer(
            &mut future,
            &SaveVersionHeader {
                mc_version: "9.9.9".to_owned(),
                data_version: 9999,
                protocol_version: 9999,
                inferred_legacy_header: false,
            },
        );
        append_world_seed_footer(&mut future, seed.unwrap());
        for target in [None, Some("1.21.1"), Some("26.3")] {
            with_target_env(target, None, || {
                let err = load_world_from(&future).unwrap_err();
                assert!(
                    err.contains("megu-world-migrate"),
                    "{err} should direct to the migration tool"
                );
            });
        }
    }
}

#[cfg(test)]
mod p5b_parity_world_border_tests {
    use super::*;

    fn custom_border_state() -> WorldBorderState {
        WorldBorderState {
            center_x: 120.5,
            center_z: -40.25,
            old_size: 300.0,
            new_size: 300.0,
            lerp_start_ms: 0,
            lerp_duration_ms: 0,
            damage_per_block: 0.5,
            damage_safe_zone: 10.0,
            warning_blocks: 20,
            warning_time_secs: 30,
            absolute_max_size: WORLD_BORDER_MAX_CENTER,
        }
    }

    fn custom_record() -> WorldBorderSave {
        WorldBorderSave {
            center_x: 120.5,
            center_z: -40.25,
            diameter: 300.0,
            damage_per_block: 0.5,
            damage_safe_zone: 10.0,
            warning_blocks: 20,
            warning_time: 30,
        }
    }

    #[test]
    fn p5b_parity_world_border_footer_round_trips_fields() {
        let mut payload = b"save-body".to_vec();
        append_world_border_footer(&mut payload, &custom_record());
        append_world_seed_footer(&mut payload, 42);
        assert_eq!(
            read_world_border_footer(&payload).unwrap(),
            Some(custom_record())
        );
        // The seed strip hides WBD1 completely: the caller sees the frame a
        // pre-record save would have ended with.
        let (rest, seed) = read_world_seed_footer(&payload).unwrap();
        assert_eq!(seed, Some(42));
        assert_eq!(rest, b"save-body");
    }

    #[test]
    fn p5b_parity_world_border_default_save_emits_no_footer() {
        let _guard = world_guard();
        let old = *world_border().lock().unwrap_or_else(|e| e.into_inner());
        *world_border().lock().unwrap_or_else(|e| e.into_inner()) = WorldBorderState::default();
        let mut payload = Vec::new();
        save_world_into(&mut payload);
        *world_border().lock().unwrap_or_else(|e| e.into_inner()) = old;
        assert!(payload.ends_with(WORLD_SEED_MAGIC));
        assert_eq!(read_world_border_footer(&payload).unwrap(), None);
        // A default save is byte-for-byte the pre-record layout: the 12-byte
        // seed trailer is the only record at the tail.
        let (rest, seed) = read_world_seed_footer(&payload).unwrap();
        assert!(seed.is_some());
        assert_eq!(rest.len() + 12, payload.len());
    }

    #[test]
    fn p5b_parity_world_border_custom_state_round_trips() {
        let _guard = world_guard();
        let old = *world_border().lock().unwrap_or_else(|e| e.into_inner());
        *world_border().lock().unwrap_or_else(|e| e.into_inner()) = custom_border_state();
        let mut payload = Vec::new();
        save_world_into(&mut payload);
        assert_eq!(
            read_world_border_footer(&payload).unwrap(),
            Some(custom_record())
        );
        *world_border().lock().unwrap_or_else(|e| e.into_inner()) = WorldBorderState::default();
        load_world_from(&payload).unwrap();
        let loaded = *world_border().lock().unwrap_or_else(|e| e.into_inner());
        *world_border().lock().unwrap_or_else(|e| e.into_inner()) = old;
        assert_eq!(
            (
                loaded.center_x,
                loaded.center_z,
                loaded.new_size,
                loaded.damage_per_block,
                loaded.damage_safe_zone,
                loaded.warning_blocks,
                loaded.warning_time_secs
            ),
            (120.5, -40.25, 300.0, 0.5, 10.0, 20, 30)
        );
    }

    #[test]
    fn p5b_parity_world_border_record_restores_without_brd1() {
        let _guard = world_guard();
        let old = *world_border().lock().unwrap_or_else(|e| e.into_inner());
        *world_border().lock().unwrap_or_else(|e| e.into_inner()) = custom_border_state();
        let mut payload = Vec::new();
        save_world_into(&mut payload);
        *world_border().lock().unwrap_or_else(|e| e.into_inner()) = WorldBorderState::default();
        // Remove the BRD1 body record (magic + 8*7 + 4*3 body bytes) so only
        // WBD1 can restore the border.
        const BRD1_LEN: usize = 4 + 8 * 7 + 4 * 3;
        let at = payload
            .windows(4)
            .position(|w| w == b"BRD1")
            .expect("BRD1 record present");
        payload.drain(at..at + BRD1_LEN);
        load_world_from(&payload).unwrap();
        let loaded = *world_border().lock().unwrap_or_else(|e| e.into_inner());
        *world_border().lock().unwrap_or_else(|e| e.into_inner()) = old;
        assert_eq!(
            (
                loaded.center_x,
                loaded.center_z,
                loaded.new_size,
                loaded.warning_blocks,
                loaded.warning_time_secs
            ),
            (120.5, -40.25, 300.0, 20, 30)
        );
    }

    #[test]
    fn p5b_parity_world_border_corrupt_footer_fails_closed() {
        // An honest non-WBD1 tail is simply "no record", never a misparse.
        let mut payload = b"save-body".to_vec();
        payload.extend_from_slice(b"XXXX");
        assert_eq!(read_world_border_footer(&payload).unwrap(), None);
        // A WBD1 trailer whose declared body cannot exist is an error.
        let mut bad = b"body".to_vec();
        bad.extend_from_slice(&999u64.to_be_bytes());
        bad.extend_from_slice(WORLD_BORDER_MAGIC);
        assert!(read_world_border_footer(&bad).is_err());
        // An unknown footer schema version is an error, not a silent skip.
        let mut bad = b"body".to_vec();
        bad.push(9);
        bad.extend_from_slice(&1u64.to_be_bytes());
        bad.extend_from_slice(WORLD_BORDER_MAGIC);
        assert!(read_world_border_footer(&bad).is_err());
        // A corrupt record inside a real save fails the whole load closed.
        let _guard = world_guard();
        let old = *world_border().lock().unwrap_or_else(|e| e.into_inner());
        *world_border().lock().unwrap_or_else(|e| e.into_inner()) = custom_border_state();
        let mut payload = Vec::new();
        save_world_into(&mut payload);
        *world_border().lock().unwrap_or_else(|e| e.into_inner()) = old;
        let at = payload
            .windows(4)
            .position(|w| w == b"WBD1")
            .expect("WBD1 record present");
        // A non-finite center is rejected by the finite-double read.
        payload[at - 48..at - 40].copy_from_slice(&f64::NAN.to_be_bytes());
        assert!(load_world_from(&payload).is_err());
    }
}

// ---------- live region-scoped chunk unloader (issue #105) ----------

/// One sweep's eviction bounds: the most unticketed chunks it may unload and
/// how many ticks a chunk keeps its residency after it was last covered by a
/// player view window, the spawn ring, or a `/forceload` ticket.
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
struct LiveChunkUnloadPolicy {
    max_unticketed_chunks: usize,
    grace_ticks: u64,
}

/// What one unload or sweep did. `blocks_flushed` counts overlay cells
/// preserved — written to `flush_dir` or parked in `evicted_regions`;
/// `block_entities_unloaded` counts reclaimed container/sign/store records;
/// `entities_unloaded` counts the non-player mobs and item drops reclaimed.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[allow(dead_code)]
struct LiveUnloadSummary {
    chunks_unloaded: usize,
    blocks_flushed: usize,
    block_entities_unloaded: usize,
    entities_unloaded: usize,
}

/// The last sweep tick a resident chunk sat inside a covered window: a
/// connected client's view, the spawn ring, or the forceload set. The sweep
/// stamps the covered chunks it sees; a resident chunk whose stamp is more
/// than `grace_ticks` old has lost its ticket and may be evicted.
#[allow(dead_code)]
static LIVE_CHUNK_TICKETS: Registry<DimChunkKey, u64> = OnceLock::new();
#[allow(dead_code)]
fn live_chunk_tickets() -> &'static LockedMap<DimChunkKey, u64> {
    LIVE_CHUNK_TICKETS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One mob drained out of the live stores for a parked chunk: the mob itself
/// plus every eid-keyed side record, so `rehydrate_evicted_chunk` puts a tamed
/// pet's owner, sitting flag, collar color and behavior back exactly as the
/// unload left them.
#[allow(dead_code)]
struct ParkedMob {
    mob: Arc<Mob>,
    live_state: Option<LiveMobState>,
    sitting_cat: bool,
    villager: Option<Villager>,
    sheared: bool,
    tags: Option<HashSet<String>>,
    attrs: Option<HashMap<String, f64>>,
    selector_attrs: Option<HashMap<String, f64>>,
}

/// Everything `park_evicted_chunk` drains out of the live stores for one
/// chunk, kept as the stores' own record types so a re-hydrate puts every
/// block, container, block entity and parked entity back exactly as the unload
/// left it.
#[derive(Default)]
#[allow(dead_code)]
struct EvictedChunkArchive {
    edits: HashMap<u32, u32>,
    chests: Vec<(DimBlockPos, HashMap<i16, Stack>)>,
    furnaces: Vec<(DimBlockPos, Furnace)>,
    brewing: Vec<(DimBlockPos, LiveBrewingStand)>,
    signs: Vec<(SignKey, SignLines)>,
    live_entities: Vec<(DimBlockPos, (i32, LiveBlockEntity))>,
    anvil_records: Vec<(DimBlockPos, AnvilNbt)>,
    crops: Vec<(DimBlockPos, u8)>,
    pending_loot: Vec<(DimBlockPos, PendingBlockLoot)>,
    mobs: Vec<ParkedMob>,
    drops: Vec<Drop>,
}

#[allow(dead_code)]
impl EvictedChunkArchive {
    /// Whether the drain carried anything at all — an archive with no records
    /// is not parked.
    fn has_records(&self) -> bool {
        !(self.edits.is_empty()
            && self.chests.is_empty()
            && self.furnaces.is_empty()
            && self.brewing.is_empty()
            && self.signs.is_empty()
            && self.live_entities.is_empty()
            && self.anvil_records.is_empty()
            && self.crops.is_empty()
            && self.pending_loot.is_empty()
            && self.mobs.is_empty()
            && self.drops.is_empty())
    }

    /// The block-entity-shaped records the drain carried: one entry per store
    /// record, so `block_entities_unloaded` counts the same cells a later
    /// flush would write.
    fn block_entity_records(&self) -> usize {
        self.chests.len()
            + self.furnaces.len()
            + self.brewing.len()
            + self.signs.len()
            + self.live_entities.len()
            + self.anvil_records.len()
            + self.pending_loot.len()
    }
}

/// The parked state of one region: chunk coordinates to its archive.
type EvictedRegionArchive = std::collections::BTreeMap<(i32, i32), EvictedChunkArchive>;

/// The compact per-region archive an unload parks its state in when no Anvil
/// directory is asked for: region coordinates key the map so "what did region
/// `(rx, rz)` lose" answers the way the `.mca` merge answers it on disk.
#[allow(dead_code)]
static EVICTED_REGIONS: Registry<(DimId, i32, i32), EvictedRegionArchive> = OnceLock::new();
#[allow(dead_code)]
fn evicted_regions() -> &'static LockedMap<(DimId, i32, i32), EvictedRegionArchive> {
    EVICTED_REGIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The spawn ring in the Overworld — the `spawnChunkRadius` gamerule's window
/// around the world-spawn chunk, the set vanilla keeps entity-ticking at all
/// times. Only the Overworld carries a world spawn.
#[allow(dead_code)]
fn live_spawn_ticket_chunks() -> HashSet<DimChunkKey> {
    let (sx, _, sz, _) = *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner());
    let radius = command_gamerules()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get("spawnChunkRadius")
        .copied()
        .unwrap_or(2)
        .clamp(0, 32);
    let mut cells = HashSet::new();
    extend_chunk_square(
        &mut cells,
        (
            (sx.floor() as i32).div_euclid(16),
            (sz.floor() as i32).div_euclid(16),
        ),
        radius,
    );
    cells
        .into_iter()
        .map(|(cx, cz)| (DimId::Overworld, cx, cz))
        .collect()
}

/// The union of every connected client's held chunk window, tagged with the
/// dimension that client is in — the live answer to "which chunks is a player
/// looking at right now".
#[allow(dead_code)]
fn live_client_view_chunks() -> HashSet<DimChunkKey> {
    clients()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .flat_map(|client| {
            let dim = *client.dim.lock().unwrap_or_else(|e| e.into_inner());
            client
                .chunks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|&(cx, cz)| (dim, cx, cz))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The ticket check applied at the point of no return. A covered chunk — any
/// connected client's held window, the spawn ring, or the forceload set —
/// must never reach eviction, so `unload_unticketed_chunk_overlays` verifies
/// it itself rather than trusting every caller's covered set to be complete.
#[allow(dead_code)]
fn live_chunk_is_ticketed(dim: DimId, cx: i32, cz: i32) -> bool {
    live_spawn_ticket_chunks().contains(&(dim, cx, cz))
        || forceloaded()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&(cx, cz))
        || live_client_view_chunks().contains(&(dim, cx, cz))
}

/// Does the chunk hold any block-entity-shaped record a flush must carry —
/// containers, furnaces, stands, signs, store entries, imported tags or a
/// pending loot roll? An empty answer with an empty overlay means the chunk
/// has nothing worth writing.
#[allow(dead_code)]
fn live_chunk_has_records(dim: DimId, cx: i32, cz: i32) -> bool {
    let in_chunk = |x: i32, z: i32| x >> 4 == cx && z >> 4 == cz;
    let block_in_chunk = |&(d, x, _, z): &DimBlockPos| d == dim && in_chunk(x, z);
    chests()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .any(|key| matches!(key, ContainerKey::Block(pos) if block_in_chunk(pos)))
        || furnaces()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(block_in_chunk)
        || live_brewing_stands()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(block_in_chunk)
        || signs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(|&(d, x, _, z, _)| d == dim && in_chunk(x, z))
        || live_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .keys()
            .any(block_in_chunk)
        || anvil_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(block_in_chunk)
        || pending_block_loot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .any(block_in_chunk)
}

/// Every chunk holding unloadable live state: overlay edits, any block-entity
/// store record, and the entities parked in it.
#[allow(dead_code)]
fn live_resident_chunks() -> HashSet<DimChunkKey> {
    let mut out: HashSet<DimChunkKey> = world()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .copied()
        .collect();
    let block_chunk = |&(d, x, _, z): &DimBlockPos| (d, x >> 4, z >> 4);
    out.extend(
        chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .filter_map(|key| match key {
                ContainerKey::Block(pos) => Some(block_chunk(pos)),
                ContainerKey::Cart(_) => None,
            }),
    );
    for keys in [
        furnaces()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect::<Vec<DimBlockPos>>(),
        live_brewing_stands()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect(),
        anvil_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect(),
        live_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .keys()
            .copied()
            .collect(),
        crops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect(),
        pending_block_loot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect(),
    ] {
        out.extend(keys.iter().map(block_chunk));
    }
    out.extend(
        signs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .map(|&(d, x, _, z, _)| (d, x >> 4, z >> 4)),
    );
    for mob in mobs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        let (x, _, z, _, _) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
        out.insert((
            mob.dim,
            (x.floor() as i32).div_euclid(16),
            (z.floor() as i32).div_euclid(16),
        ));
    }
    // Drops carry no dimension in this store; they are parked with the
    // Overworld chunk their position falls in, the same dim the legacy save
    // section writes them under.
    for item_drop in drops().lock().unwrap_or_else(|e| e.into_inner()).iter() {
        out.insert((
            DimId::Overworld,
            (item_drop.x.floor() as i32).div_euclid(16),
            (item_drop.z.floor() as i32).div_euclid(16),
        ));
    }
    out
}

/// Drain `(dim, cx, cz)` out of every live store into an archive. The caller
/// decides whether the archive is parked in `evicted_regions` or dropped after
/// a disk flush — either way the live copies are gone, which is what
/// "unloaded" means. Derived per-chunk caches (`op_bands`, `section_store`,
/// `chunk_versions`) are dropped too: they rebuild on demand and their memory
/// is the exploration memory this unloader exists to reclaim.
#[allow(dead_code)]
fn park_evicted_chunk(dim: DimId, cx: i32, cz: i32) -> EvictedChunkArchive {
    let in_chunk = |x: i32, z: i32| x >> 4 == cx && z >> 4 == cz;
    let mut archive = EvictedChunkArchive {
        edits: world()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(dim, cx, cz))
            .unwrap_or_default(),
        ..EvictedChunkArchive::default()
    };
    {
        let mut store = chests().lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<ContainerKey> = store
            .keys()
            .copied()
            .filter(|key| {
                matches!(key, ContainerKey::Block((d, x, _, z)) if *d == dim && in_chunk(*x, *z))
            })
            .collect();
        for key in keys {
            if let (ContainerKey::Block(pos), Some(slots)) = (key, store.remove(&key)) {
                archive.chests.push((pos, slots));
            }
        }
    }
    {
        let mut store = furnaces().lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<DimBlockPos> = store
            .keys()
            .copied()
            .filter(|&(d, x, _, z)| d == dim && in_chunk(x, z))
            .collect();
        for key in keys {
            if let Some(furnace) = store.remove(&key) {
                archive.furnaces.push((key, furnace));
            }
        }
    }
    {
        let mut store = live_brewing_stands()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let keys: Vec<DimBlockPos> = store
            .keys()
            .copied()
            .filter(|&(d, x, _, z)| d == dim && in_chunk(x, z))
            .collect();
        for key in keys {
            if let Some(stand) = store.remove(&key) {
                archive.brewing.push((key, stand));
            }
        }
    }
    {
        let mut store = signs().lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<SignKey> = store
            .keys()
            .copied()
            .filter(|&(d, x, _, z, _)| d == dim && in_chunk(x, z))
            .collect();
        for key in keys {
            if let Some(lines) = store.remove(&key) {
                archive.signs.push((key, lines));
            }
        }
    }
    {
        let mut store = live_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let keys: Vec<DimBlockPos> = store
            .entries
            .keys()
            .copied()
            .filter(|&(d, x, _, z)| d == dim && in_chunk(x, z))
            .collect();
        for key in keys {
            if let Some(entry) = store.entries.remove(&key) {
                archive.live_entities.push((key, entry));
            }
        }
        store.chunk_versions.remove(&(dim, cx, cz));
    }
    {
        let mut store = anvil_block_entities()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let keys: Vec<DimBlockPos> = store
            .keys()
            .copied()
            .filter(|&(d, x, _, z)| d == dim && in_chunk(x, z))
            .collect();
        for key in keys {
            if let Some(tag) = store.remove(&key) {
                archive.anvil_records.push((key, tag));
            }
        }
    }
    {
        let mut store = crops().lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<DimBlockPos> = store
            .keys()
            .copied()
            .filter(|&(d, x, _, z)| d == dim && in_chunk(x, z))
            .collect();
        for key in keys {
            if let Some(age) = store.remove(&key) {
                archive.crops.push((key, age));
            }
        }
    }
    {
        let mut store = pending_block_loot()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let keys: Vec<DimBlockPos> = store
            .keys()
            .copied()
            .filter(|&(d, x, _, z)| d == dim && in_chunk(x, z))
            .collect();
        for key in keys {
            if let Some(loot) = store.remove(&key) {
                archive.pending_loot.push((key, loot));
            }
        }
    }
    chunk_versions()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(dim, cx, cz));
    op_bands()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map
        .remove(&(dim, cx, cz));
    section_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map
        .remove(&(dim, cx, cz));
    archive
}

/// Drain the non-player entities a chunk held: mobs keyed by dimension, plus
/// item drops, which this store keeps dimension-agnostic and which are
/// therefore reclaimed only with the Overworld chunk their position falls in.
/// Connected players are never in either store — `clients()` is a separate
/// list the unloader never touches. The drained records come back to the
/// caller so a park keeps them instead of losing them outright; each mob's
/// eid-keyed side records (behavior state, sitting flag, villager data,
/// shearing, tags, attributes) travel with it.
#[allow(dead_code)]
fn drain_chunk_entities(dim: DimId, cx: i32, cz: i32) -> (usize, Vec<ParkedMob>, Vec<Drop>) {
    let mut parked_mobs = Vec::new();
    {
        let mut ms = mobs().lock().unwrap_or_else(|e| e.into_inner());
        let mut i = 0;
        while i < ms.len() {
            let in_chunk = ms[i].dim == dim && {
                let (x, _, z, _, _) = *ms[i].pos.lock().unwrap_or_else(|e| e.into_inner());
                (x.floor() as i32).div_euclid(16) == cx
                    && (z.floor() as i32).div_euclid(16) == cz
            };
            if in_chunk {
                parked_mobs.push(ms.remove(i));
            } else {
                i += 1;
            }
        }
    }
    // One store lock per statement: a `MutexGuard` temporary inside a struct
    // literal lives to the end of the whole expression, which would hold every
    // store at once — and re-locking `attrs()` there deadlocks.
    let parked_mobs: Vec<ParkedMob> = parked_mobs
        .into_iter()
        .map(|mob| {
            let eid = mob.eid;
            let live_state = live_mob_states()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&eid);
            let sitting_cat = sitting_cats()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&eid);
            let villager = live_villagers()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&eid);
            let sheared = sheared()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&eid);
            let tags = tags()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&format!("e:{eid}"));
            let (attrs_map, selector_attrs) = {
                let mut attrs = attrs().lock().unwrap_or_else(|e| e.into_inner());
                (
                    attrs.remove(&format!("e:{eid}")),
                    attrs.remove(&format!("e:m:{eid}")),
                )
            };
            ParkedMob {
                mob,
                live_state,
                sitting_cat,
                villager,
                sheared,
                tags,
                attrs: attrs_map,
                selector_attrs,
            }
        })
        .collect();
    let mut parked_drops = Vec::new();
    if dim == DimId::Overworld {
        let mut ds = drops().lock().unwrap_or_else(|e| e.into_inner());
        let mut i = 0;
        while i < ds.len() {
            let in_chunk = (ds[i].x.floor() as i32).div_euclid(16) == cx
                && (ds[i].z.floor() as i32).div_euclid(16) == cz;
            if in_chunk {
                parked_drops.push(ds.remove(i));
            } else {
                i += 1;
            }
        }
    }
    (parked_mobs.len() + parked_drops.len(), parked_mobs, parked_drops)
}

/// Evict one unticketed chunk: flush its dirty overlay and block entities to
/// `flush_dir` (or park them in `evicted_regions` when no directory is given),
/// then reclaim the live overlay cells, container stores, block entities and
/// non-player entities the chunk held. The flush runs before anything is
/// removed, so a failed write aborts the unload, the chunk stays resident and
/// no edit is ever lost. A chunk still covered by a player window, the spawn
/// ring or `/forceload` is refused outright.
#[allow(dead_code)]
fn unload_unticketed_chunk_overlays(
    dim: DimId,
    cx: i32,
    cz: i32,
    flush_dir: Option<&std::path::Path>,
) -> Result<LiveUnloadSummary, String> {
    if live_chunk_is_ticketed(dim, cx, cz) {
        return Err(format!("chunk ({dim:?}, {cx}, {cz}) is still ticketed"));
    }
    with_save_guard(|| {
        let mut summary = LiveUnloadSummary::default();
        if let Some(dir) = flush_dir {
            // The flush reads the live stores, so it runs before the drain;
            // a failed write leaves the chunk resident and loses nothing.
            let dirty = world()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&(dim, cx, cz))
                .is_some_and(|edits| !edits.is_empty())
                || live_chunk_has_records(dim, cx, cz);
            if dirty {
                summary.blocks_flushed = flush_single_chunk_to_anvil_dir(dir, dim, cx, cz)?;
            }
            let dropped = park_evicted_chunk(dim, cx, cz);
            summary.block_entities_unloaded = dropped.block_entity_records();
            // The flush preserved the edits; the dropped archive is only the
            // drain record, so count the cells the flush wrote.
            debug_assert_eq!(summary.blocks_flushed, dropped.edits.len());
            // A disk flush carries only blocks and block entities, so the
            // drained entities are reclaimed outright.
            summary.entities_unloaded = drain_chunk_entities(dim, cx, cz).0;
        } else {
            let mut archive = park_evicted_chunk(dim, cx, cz);
            let (entities, mobs, item_drops) = drain_chunk_entities(dim, cx, cz);
            archive.mobs = mobs;
            archive.drops = item_drops;
            summary.entities_unloaded = entities;
            summary.block_entities_unloaded = archive.block_entity_records();
            summary.blocks_flushed = archive.edits.len();
            if archive.has_records() {
                evicted_regions()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry((dim, cx.div_euclid(32), cz.div_euclid(32)))
                    .or_default()
                    .insert((cx, cz), archive);
            }
        }
        live_chunk_tickets()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(dim, cx, cz));
        if summary.blocks_flushed > 0
            || summary.block_entities_unloaded > 0
            || summary.entities_unloaded > 0
        {
            summary.chunks_unloaded = 1;
        }
        Ok(summary)
    })
}

/// Evict resident chunks that have lost their ticket. A chunk covered by an
/// active player view window (the caller's set plus every live client's held
/// window), the Overworld spawn ring or `/forceload` is stamped and never
/// unloaded; the rest age by `grace_ticks` from their last stamp and are
/// unloaded up to `max_unticketed_chunks` per call. Each unload preserves its
/// blocks to `flush_dir` (or `evicted_regions`) before reclaiming, so a failed
/// write aborts that chunk's eviction rather than losing an edit.
#[allow(dead_code)]
fn sweep_unticketed_regions(
    active_player_chunks: &HashSet<DimChunkKey>,
    forceloaded_chunks: &HashSet<(i32, i32)>,
    policy: LiveChunkUnloadPolicy,
    current_tick: u64,
    flush_dir: Option<&std::path::Path>,
) -> Result<LiveUnloadSummary, String> {
    let mut covered = live_client_view_chunks();
    covered.extend(active_player_chunks.iter().copied());
    // The forceload set is dimension-agnostic server state — a ticket at
    // `(cx, cz)` covers that chunk in every registered dimension.
    {
        let forced = forceloaded().lock().unwrap_or_else(|e| e.into_inner());
        for dim in DimId::all_registered() {
            for &(cx, cz) in forceloaded_chunks.iter().chain(forced.iter()) {
                covered.insert((dim, cx, cz));
            }
        }
    }
    covered.extend(live_spawn_ticket_chunks());
    let mut residents = live_resident_chunks();
    {
        let mut tickets = live_chunk_tickets()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for key in covered.intersection(&residents) {
            tickets.insert(*key, current_tick);
        }
    }
    residents.retain(|key| !covered.contains(key));
    let mut candidates: Vec<DimChunkKey> = residents.into_iter().collect();
    candidates.sort();
    let mut summary = LiveUnloadSummary::default();
    for (dim, cx, cz) in candidates {
        if summary.chunks_unloaded >= policy.max_unticketed_chunks {
            break;
        }
        let last = live_chunk_tickets()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(dim, cx, cz))
            .copied()
            .unwrap_or(0);
        if current_tick.saturating_sub(last) < policy.grace_ticks {
            continue;
        }
        let step = unload_unticketed_chunk_overlays(dim, cx, cz, flush_dir)?;
        summary.chunks_unloaded += step.chunks_unloaded;
        summary.blocks_flushed += step.blocks_flushed;
        summary.block_entities_unloaded += step.block_entities_unloaded;
        summary.entities_unloaded += step.entities_unloaded;
    }
    Ok(summary)
}

/// Re-hydrate a chunk parked in `evicted_regions`: the parked overlay and
/// every drained store record returns to the live world exactly as the unload
/// left it, with the derived caches left to rebuild on demand.
#[allow(dead_code)]
fn rehydrate_evicted_chunk(dim: DimId, cx: i32, cz: i32) -> bool {
    let archive = {
        let mut regions = evicted_regions()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let region_key = (dim, cx.div_euclid(32), cz.div_euclid(32));
        let archive = regions
            .get_mut(&region_key)
            .and_then(|region| region.remove(&(cx, cz)));
        if regions
            .get(&region_key)
            .is_some_and(|region| region.is_empty())
        {
            regions.remove(&region_key);
        }
        archive
    };
    let Some(archive) = archive else {
        return false;
    };
    with_save_guard(|| {
        if !archive.edits.is_empty() {
            world()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((dim, cx, cz), archive.edits);
        }
        for (pos, slots) in archive.chests {
            chests()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(ContainerKey::Block(pos), slots);
        }
        for (pos, furnace) in archive.furnaces {
            furnaces()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pos, furnace);
        }
        for (pos, stand) in archive.brewing {
            live_brewing_stands()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pos, stand);
        }
        for (key, lines) in archive.signs {
            signs()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, lines);
        }
        {
            let mut store = live_block_entities()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for (pos, entry) in archive.live_entities {
                store.entries.insert(pos, entry);
            }
        }
        for (pos, tag) in archive.anvil_records {
            anvil_block_entities()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pos, tag);
        }
        for (pos, age) in archive.crops {
            crops()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pos, age);
        }
        for (pos, loot) in archive.pending_loot {
            pending_block_loot()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pos, loot);
        }
        for parked in archive.mobs {
            let eid = parked.mob.eid;
            if let Some(state) = parked.live_state {
                live_mob_states()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(eid, state);
            }
            if parked.sitting_cat {
                sitting_cats()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(eid);
            }
            if let Some(villager) = parked.villager {
                live_villagers()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(eid, villager);
            }
            if parked.sheared {
                sheared()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(eid);
            }
            if let Some(tag_set) = parked.tags {
                tags()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(format!("e:{eid}"), tag_set);
            }
            {
                let mut attrs = attrs().lock().unwrap_or_else(|e| e.into_inner());
                if let Some(values) = parked.attrs {
                    attrs.insert(format!("e:{eid}"), values);
                }
                if let Some(values) = parked.selector_attrs {
                    attrs.insert(format!("e:m:{eid}"), values);
                }
            }
            mobs()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(parked.mob);
        }
        drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(archive.drops);
    });
    true
}

/// Re-hydrate a chunk the unloader evicted: with a `flush_dir` the merged
/// `.mca` record answers, without one the parked `evicted_regions` archive
/// does — either way edited blocks and containers come back byte-for-byte.
#[allow(dead_code)]
fn reload_unticketed_chunk_overlay(
    dim: DimId,
    cx: i32,
    cz: i32,
    flush_dir: Option<&std::path::Path>,
) -> Result<bool, String> {
    match flush_dir {
        Some(dir) => reload_single_chunk_from_anvil_dir(dir, dim, cx, cz),
        None => Ok(rehydrate_evicted_chunk(dim, cx, cz)),
    }
}

#[cfg(all(test, megu_vanilla_data))]
mod p8b4_issue105_tests {
    use super::*;

    fn p8b4_mob(eid: i32, x: f64, y: f64, z: f64) -> Arc<Mob> {
        Arc::new(Mob {
            dim: DimId::Overworld,
            eid,
            uuid: [eid as u8; 16],
            etype: 1,
            pos: Mutex::new((x, y, z, 0.0, 0.0)),
            hp: Mutex::new(10.0),
            last_hit: Mutex::new(Instant::now()),
            hurt_at: Mutex::new(Instant::now()),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(60),
        })
    }

    fn p8b4_overlay_len(key: DimChunkKey) -> usize {
        world()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .map_or(0, HashMap::len)
    }

    fn p8b4_total_overlay_cells() -> usize {
        world()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(HashMap::len)
            .sum()
    }

    #[test]
    fn p8b4_issue105_sweep_unloads_unticketed_chunk_and_preserves_edits() {
        if test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        test_support::require_vanilla_data();
        let dir =
            std::env::temp_dir().join(format!("megu-p8b4-flush-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let stone = name_to_state("minecraft:stone").expect("stone state");
        let dirt = name_to_state("minecraft:dirt").expect("dirt state");
        let glass = name_to_state("minecraft:glass").expect("glass state");
        let states = [stone, dirt, glass];
        // 200 edits, a stocked chest and a mob in far chunk (50, 50); the
        // spawn chunk (0, 0) keeps an edit of its own under a live ticket.
        let mut edits = Vec::new();
        for i in 0..200i32 {
            let (x, y, z) = (800 + (i % 16), 64 + (i / 16), 800 + (i % 3));
            let st = states[(i as usize) % states.len()];
            set_block_dim(DimId::Overworld, x, y, z, st);
            edits.push(((x, y, z), st));
        }
        let chest_pos: DimBlockPos = (DimId::Overworld, 803, 70, 803);
        let apple = name_to_item("minecraft:apple").expect("apple item");
        chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                ContainerKey::Block(chest_pos),
                HashMap::from([(4i16, (apple, 3u8, Vec::new()))]),
            );
        mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(p8b4_mob(900_001, 808.5, 70.0, 808.5));
        set_block_dim(DimId::Overworld, 2, 64, 3, stone);
        assert_eq!(p8b4_overlay_len((DimId::Overworld, 50, 50)), 200);
        let cells_before = p8b4_total_overlay_cells();

        let active: HashSet<DimChunkKey> = HashSet::from([(DimId::Overworld, 0, 0)]);
        let summary = sweep_unticketed_regions(
            &active,
            &HashSet::new(),
            LiveChunkUnloadPolicy {
                max_unticketed_chunks: 64,
                grace_ticks: 0,
            },
            1,
            Some(&dir),
        )
        .unwrap();
        assert_eq!(summary.chunks_unloaded, 1, "only the unticketed far chunk unloads");
        assert_eq!(summary.blocks_flushed, 200);
        assert!(summary.block_entities_unloaded >= 1, "chest record reclaimed");
        assert!(summary.entities_unloaded >= 1, "mob reclaimed");
        assert_eq!(p8b4_overlay_len((DimId::Overworld, 50, 50)), 0);
        assert_eq!(
            p8b4_total_overlay_cells(),
            cells_before - 200,
            "the overlay dropped exactly the evicted cells"
        );
        assert_eq!(
            p8b4_overlay_len((DimId::Overworld, 0, 0)),
            1,
            "spawn chunk 100% untouched"
        );
        assert!(chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        assert!(mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        assert!(dir.join("region").join("r.1.1.mca").is_file());

        // Re-hydrate from the flushed Anvil dir: every edited block returns
        // byte-for-byte and the chest gets its slots back.
        assert!(
            reload_single_chunk_from_anvil_dir(&dir, DimId::Overworld, 50, 50).unwrap()
        );
        for ((x, y, z), st) in &edits {
            assert_eq!(
                block_at_dim(DimId::Overworld, *x, *y, *z),
                *st,
                "edit at ({x}, {y}, {z})"
            );
        }
        let restored = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ContainerKey::Block(chest_pos))
            .cloned()
            .unwrap_or_default();
        assert_eq!(restored.get(&4), Some(&(apple, 3, Vec::new())));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p8b4_issue105_evicted_region_archive_rehydrates_blocks_and_chest() {
        if test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        let stone = name_to_state("minecraft:stone").unwrap_or(7);
        let dirt = name_to_state("minecraft:dirt").unwrap_or(11);
        set_block_dim(DimId::Overworld, 640, 64, 640, stone);
        set_block_dim(DimId::Overworld, 641, 64, 641, dirt);
        let chest_pos: DimBlockPos = (DimId::Overworld, 642, 70, 642);
        let apple = name_to_item("minecraft:apple").unwrap_or(1);
        chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                ContainerKey::Block(chest_pos),
                HashMap::from([(0i16, (apple, 2u8, Vec::new()))]),
            );
        mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(p8b4_mob(900_002, 645.5, 70.0, 645.5));

        // No flush dir: the per-region evicted archive is the preservation path.
        let summary = sweep_unticketed_regions(
            &HashSet::new(),
            &HashSet::new(),
            LiveChunkUnloadPolicy {
                max_unticketed_chunks: 64,
                grace_ticks: 0,
            },
            1,
            None,
        )
        .unwrap();
        assert_eq!(summary.chunks_unloaded, 1);
        assert_eq!(summary.blocks_flushed, 2);
        assert!(summary.block_entities_unloaded >= 1);
        assert!(summary.entities_unloaded >= 1);
        assert_eq!(p8b4_overlay_len((DimId::Overworld, 40, 40)), 0);
        assert!(evicted_regions()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(DimId::Overworld, 1, 1))
            .is_some_and(|region| region.contains_key(&(40, 40))));

        assert!(
            reload_unticketed_chunk_overlay(DimId::Overworld, 40, 40, None).unwrap(),
            "archive re-hydrates the chunk"
        );
        assert_eq!(block_at_dim(DimId::Overworld, 640, 64, 640), stone);
        assert_eq!(block_at_dim(DimId::Overworld, 641, 64, 641), dirt);
        let restored = chests()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&ContainerKey::Block(chest_pos))
            .cloned()
            .unwrap_or_default();
        assert_eq!(restored.get(&0), Some(&(apple, 2, Vec::new())));
    }

    #[test]
    fn p8b4_issue105_forceloaded_chunk_is_never_unloaded() {
        if test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        let stone = name_to_state("minecraft:stone").unwrap_or(7);
        set_block_dim(DimId::Overworld, 800, 64, 800, stone);
        let forced: HashSet<(i32, i32)> = HashSet::from([(50, 50)]);
        let summary = sweep_unticketed_regions(
            &HashSet::new(),
            &forced,
            LiveChunkUnloadPolicy {
                max_unticketed_chunks: 64,
                grace_ticks: 0,
            },
            1,
            None,
        )
        .unwrap();
        assert_eq!(summary.chunks_unloaded, 0);
        assert_eq!(p8b4_overlay_len((DimId::Overworld, 50, 50)), 1);
        // The primitive itself also refuses the chunk while the ticket lives.
        forceloaded()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((50, 50));
        assert!(unload_unticketed_chunk_overlays(DimId::Overworld, 50, 50, None).is_err());
        assert_eq!(p8b4_overlay_len((DimId::Overworld, 50, 50)), 1);
    }
}

#[cfg(test)]
mod fix_save_tests {
    use super::*;

    /// Insert a tamed pet into the live stores the way `insert_live_mob` +
    /// `PetBehavior::try_tame` leave them, returning its eid.
    fn fix_save_pet(etype: i64, x: f64, z: f64, owner: [u8; 16], item: &str, collar: u8) -> i32 {
        assert!(insert_live_mob(
            etype,
            DimId::Overworld,
            (x, 70.0, z, 0.0, 0.0),
            8.0,
            None
        ));
        let eid = {
            let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
            mobs.last().unwrap().eid
        };
        {
            let mut states = live_mob_states().lock().unwrap_or_else(|e| e.into_inner());
            let pet = match &mut states.get_mut(&eid).unwrap().behavior {
                Some(MobBehaviorState::Wolf(p)) | Some(MobBehaviorState::Cat(p)) => p,
                _ => unreachable!("pet behavior for etype {etype}"),
            };
            assert_eq!(pet.try_tame(owner, item, &mut |_| 0), PetTameResult::Tamed);
            assert!(pet.dye_collar(owner, collar));
            assert!(pet.sitting);
        }
        // `try_tame` bumps the behavior record's hp to the tamed max; mirror it
        // into the mob's live hp the way the taming interaction does.
        let max_hp = {
            let states = live_mob_states().lock().unwrap_or_else(|e| e.into_inner());
            match &states[&eid].behavior {
                Some(MobBehaviorState::Wolf(p)) | Some(MobBehaviorState::Cat(p)) => p.hp,
                _ => unreachable!(),
            }
        };
        let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
        let mob = mobs.iter().find(|m| m.eid == eid).unwrap();
        *mob.hp.lock().unwrap_or_else(|e| e.into_inner()) = max_hp;
        eid
    }

    /// The live record of `eid`'s pet behavior: `(owner, sitting, collar, pet
    /// hp, mob hp, is_wolf)`.
    fn fix_save_pet_record(eid: i32) -> (Option<[u8; 16]>, bool, u8, f32, f32, bool) {
        let states = live_mob_states().lock().unwrap_or_else(|e| e.into_inner());
        let (pet, is_wolf) = match &states[&eid].behavior {
            Some(MobBehaviorState::Wolf(p)) => (p, true),
            Some(MobBehaviorState::Cat(p)) => (p, false),
            _ => unreachable!(),
        };
        let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
        let mob = mobs.iter().find(|m| m.eid == eid).unwrap();
        let mob_hp = saved_value(&mob.hp);
        (pet.owner, pet.sitting, pet.collar_color, pet.hp, mob_hp, is_wolf)
    }

    #[test]
    fn fix_save_tamed_wolf_and_cat_roundtrip_save_v2() {
        if test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        let owner = [0x5Au8; 16];
        let wolf = fix_save_pet(122, 50.5, 50.5, owner, "bone", 11);
        let cat = fix_save_pet(15, 51.5, 51.5, owner, "cod", 3);
        assert_eq!(fix_save_pet_record(wolf), (Some(owner), true, 11, 40.0, 40.0, true));
        assert_eq!(fix_save_pet_record(cat), (Some(owner), true, 3, 10.0, 10.0, false));

        let mut payload = Vec::new();
        save_world_into(&mut payload);
        mobs().lock().unwrap_or_else(|e| e.into_inner()).clear();
        live_mob_states().lock().unwrap_or_else(|e| e.into_inner()).clear();
        load_world_from(&payload).unwrap();

        // eids are reissued on load: find the restored pets by entity type.
        let restored = mobs().lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(restored.len(), 2);
        let wolf_eid = restored.iter().find(|m| m.etype == 122).unwrap().eid;
        let cat_eid = restored.iter().find(|m| m.etype == 15).unwrap().eid;
        assert_eq!(
            fix_save_pet_record(wolf_eid),
            (Some(owner), true, 11, 40.0, 40.0, true),
            "tamed wolf survives save/load"
        );
        assert_eq!(
            fix_save_pet_record(cat_eid),
            (Some(owner), true, 3, 10.0, 10.0, false),
            "tamed cat survives save/load"
        );
    }

    #[test]
    fn fix_save_tamed_pets_survive_chunk_eviction_and_rehydrate() {
        if test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        let owner = [0x7Eu8; 16];
        // Chunk (40, 40): block coords 640..655.
        let wolf = fix_save_pet(122, 644.5, 644.5, owner, "bone", 11);
        let cat = fix_save_pet(15, 645.5, 645.5, owner, "cod", 9);

        let summary = sweep_unticketed_regions(
            &HashSet::new(),
            &HashSet::new(),
            LiveChunkUnloadPolicy {
                max_unticketed_chunks: 64,
                grace_ticks: 0,
            },
            1,
            None,
        )
        .unwrap();
        assert_eq!(summary.chunks_unloaded, 1, "the pets' chunk evicts");
        assert_eq!(summary.entities_unloaded, 2);
        assert!(mobs().lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        assert!(live_mob_states()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());

        assert!(
            reload_unticketed_chunk_overlay(DimId::Overworld, 40, 40, None).unwrap(),
            "the parked archive re-hydrates"
        );
        // The same records return under the same eids: an archive parks the
        // mob itself, not a fresh spawn.
        assert_eq!(mobs().lock().unwrap_or_else(|e| e.into_inner()).len(), 2);
        assert_eq!(
            fix_save_pet_record(wolf),
            (Some(owner), true, 11, 40.0, 40.0, true),
            "tamed wolf survives eviction"
        );
        assert_eq!(
            fix_save_pet_record(cat),
            (Some(owner), true, 9, 10.0, 10.0, false),
            "tamed cat survives eviction"
        );
    }

    #[test]
    fn fix_save_wbe_footer_omitted_when_registry_empty() {
        if test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        world_block_entity_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let mut out = Vec::new();
        append_wbe_footer(&mut out);
        assert!(out.is_empty(), "empty W-BE registry emits zero bytes");
        // The full save payload carries no WBE1 marker either.
        let mut payload = Vec::new();
        save_world_into(&mut payload);
        assert!(
            !payload.windows(4).any(|w| w == WBE_MAGIC),
            "no WBE1 footer in an untouched save"
        );
        // And the footer reader accepts its absence without touching bytes.
        let (rest, entries) = read_wbe_footer(&payload).unwrap();
        assert!(entries.is_empty());
        assert_eq!(rest.len(), payload.len());
    }

    #[test]
    fn fix_save_wbe_footer_codec_roundtrip_and_rejects_corruption() {
        if test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        {
            let mut registry = world_block_entity_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            registry.insert(
                (DimId::Overworld, 10, 64, 10),
                VanillaBlockEntity::Campfire(CampfireBlockEntity {
                    slots: [
                        Some(("minecraft:beef".to_string(), 40, 600)),
                        Some(("minecraft:cod".to_string(), 120, 600)),
                        None,
                        None,
                    ],
                }),
            );
            registry.insert(
                (DimId::Nether, -8, 70, 33),
                VanillaBlockEntity::Jukebox(JukeboxBlockEntity {
                    record_item: Some("minecraft:music_disc_cat".to_string()),
                    ticks_since_song_started: 1234,
                }),
            );
            registry.insert(
                (DimId::End, 100, 49, -7),
                VanillaBlockEntity::Lectern(LecternBlockEntity {
                    book_item: Some("minecraft:writable_book".to_string()),
                    page: 3,
                    total_pages: 20,
                }),
            );
        }
        let mut out = Vec::new();
        append_wbe_footer(&mut out);
        assert!(out.ends_with(WBE_MAGIC));
        let (rest, entries) = read_wbe_footer(&out).unwrap();
        assert!(rest.is_empty());
        assert_eq!(entries.len(), 3);
        let campfire = entries
            .iter()
            .find(|(pos, _)| *pos == (DimId::Overworld, 10, 64, 10))
            .map(|(_, e)| e);
        let Some(VanillaBlockEntity::Campfire(campfire)) = campfire else {
            panic!("campfire missing");
        };
        assert_eq!(campfire.slots[0], Some(("minecraft:beef".to_string(), 40, 600)));
        assert_eq!(campfire.slots[1], Some(("minecraft:cod".to_string(), 120, 600)));
        assert_eq!(campfire.slots[2], None);
        let Some(VanillaBlockEntity::Jukebox(jukebox)) = entries
            .iter()
            .find(|(pos, _)| *pos == (DimId::Nether, -8, 70, 33))
            .map(|(_, e)| e)
        else {
            panic!("jukebox missing");
        };
        assert_eq!(jukebox.record_item.as_deref(), Some("minecraft:music_disc_cat"));
        assert_eq!(jukebox.ticks_since_song_started, 1234);
        let Some(VanillaBlockEntity::Lectern(lectern)) = entries
            .iter()
            .find(|(pos, _)| *pos == (DimId::End, 100, 49, -7))
            .map(|(_, e)| e)
        else {
            panic!("lectern missing");
        };
        assert_eq!(lectern.book_item.as_deref(), Some("minecraft:writable_book"));
        assert_eq!(lectern.page, 3);
        assert_eq!(lectern.total_pages, 20);

        // Malformed records fail closed, never panic. The footer is
        // trailer-framed, so "truncated" means a body shorter than its
        // declared length or a severed trailer — both reject cleanly.
        let len_pos = out.len() - 12;
        let body_len =
            u64::from_be_bytes(out[len_pos..len_pos + 8].try_into().unwrap()) as usize;
        let body_start = out.len() - 12 - body_len;
        // An inflated declared length overruns the payload.
        let mut corrupt = out.clone();
        corrupt[len_pos..len_pos + 8]
            .copy_from_slice(&(body_len as u64 + 4096).to_be_bytes());
        assert!(read_wbe_footer(&corrupt).is_err(), "overrun body length");
        // An unknown kind tag is rejected. The first entry's kind byte sits
        // after 4B count + 2B key len + key + 12B position.
        let key_len =
            u16::from_be_bytes(out[body_start + 4..body_start + 6].try_into().unwrap())
                as usize;
        let mut corrupt = out.clone();
        corrupt[body_start + 4 + 2 + key_len + 12] = 0xEE;
        assert!(read_wbe_footer(&corrupt).is_err(), "unknown kind tag");
        // A body cut mid-entry, re-framed with an honest length, cannot parse.
        let cut_body = &out[body_start..body_start + body_len - 2];
        let mut reframed = out[..body_start].to_vec();
        reframed.extend_from_slice(cut_body);
        reframed.extend_from_slice(&(cut_body.len() as u64).to_be_bytes());
        reframed.extend_from_slice(WBE_MAGIC);
        assert!(read_wbe_footer(&reframed).is_err(), "mid-entry truncation");
        // A garbage byte left after the last record inside the declared body
        // is rejected as trailing junk.
        let mut padded = out[..body_start + body_len].to_vec();
        padded.push(0xFF);
        padded.extend_from_slice(&((body_len + 1) as u64).to_be_bytes());
        padded.extend_from_slice(WBE_MAGIC);
        assert!(read_wbe_footer(&padded).is_err(), "trailing bytes in body");
    }

    #[test]
    fn fix_save_wbe_registry_survives_save_v2_roundtrip() {
        if test_support::isolated_world_test() {
            return;
        }
        let _guard = world_guard();
        {
            let mut registry = world_block_entity_registry()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            registry.insert(
                (DimId::Overworld, 10, 64, 10),
                VanillaBlockEntity::Campfire(CampfireBlockEntity {
                    slots: [Some(("minecraft:porkchop".to_string(), 5, 600)), None, None, None],
                }),
            );
            registry.insert(
                (DimId::Nether, -8, 70, 33),
                VanillaBlockEntity::Jukebox(JukeboxBlockEntity {
                    record_item: Some("minecraft:music_disc_blocks".to_string()),
                    ticks_since_song_started: 77,
                }),
            );
            registry.insert(
                (DimId::Overworld, 11, 64, 10),
                VanillaBlockEntity::Lectern(LecternBlockEntity {
                    book_item: Some("minecraft:written_book".to_string()),
                    page: 1,
                    total_pages: 9,
                }),
            );
        }
        let mut payload = Vec::new();
        save_world_into(&mut payload);
        assert!(
            payload.windows(4).any(|w| w == WBE_MAGIC),
            "a non-empty W-BE registry writes the footer"
        );
        world_block_entity_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        load_world_from(&payload).unwrap();
        let registry = world_block_entity_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(registry.len(), 3);
        let Some(VanillaBlockEntity::Campfire(campfire)) =
            registry.get(&(DimId::Overworld, 10, 64, 10))
        else {
            panic!("campfire missing after load");
        };
        assert_eq!(campfire.slots[0], Some(("minecraft:porkchop".to_string(), 5, 600)));
        let Some(VanillaBlockEntity::Jukebox(jukebox)) =
            registry.get(&(DimId::Nether, -8, 70, 33))
        else {
            panic!("jukebox missing after load");
        };
        assert_eq!(jukebox.record_item.as_deref(), Some("minecraft:music_disc_blocks"));
        assert_eq!(jukebox.ticks_since_song_started, 77);
        let Some(VanillaBlockEntity::Lectern(lectern)) =
            registry.get(&(DimId::Overworld, 11, 64, 10))
        else {
            panic!("lectern missing after load");
        };
        assert_eq!(lectern.book_item.as_deref(), Some("minecraft:written_book"));
        assert_eq!(lectern.page, 1);
        assert_eq!(lectern.total_pages, 9);
    }
}
