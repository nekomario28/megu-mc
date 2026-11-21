// mc-spine v0.6 — joinable 1.21.1 (protocol 767) flat server:
// handshake -> status | login -> config (vanilla blob replay) -> play
// (join_game + 9x9 chunks + teleport + health) -> consume loop with
// creative block dig/place backed by a shared overlay world.
use crate::block_registry::breg;
use crate::json::{jparse, J};
use crate::worldgen::DimId;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
mod cached_lookup;
mod chunk_ack;
mod connection_limits;
mod effects;
#[cfg(feature = "java-compat")]
mod java_compat;
mod landing;
mod landing_types;
#[cfg(test)]
mod light_pack_tests;
use cached_lookup::CachedLookup;
use chunk_ack::ChunkFeedback;
mod block_registry;
mod feature;
mod generation_context;
mod generation_leaf_queue;
mod generation_math;
mod generation_providers;
mod ingress_guard;
mod json;
mod tick_stats;
mod trace_env;
mod tree;
mod world_rng;
mod worldgen;
include!("paletted.rs");
include!("server_config.rs");
include!("server_ops.rs");
include!("server_reliability.rs");
include!("operator_ux.rs");
include!("anticheat.rs");
include!("lighting.rs");
include!("entity_collision.rs");
include!("entity_fluid.rs");
include!("entity_spatial.rs");
include!("entity_push.rs");
include!("entity_registry.rs");
include!("mob_ai.rs");
include!("natural_spawner.rs");
include!("redstone.rs");
include!("villager_raid.rs");
include!("player_mechanisms.rs");
include!("mob_behaviors.rs");
include!("world_mechanisms.rs");
include!("save_v2.rs");
include!("commands_core.rs");
include!("commands_execute.rs");
include!("teams_and_commands.rs");
include!("menus_and_items.rs");
include!("workstations_and_mounts.rs");
include!("combat_and_health.rs");
include!("play_protocol.rs");
include!("lod_protocol.rs");
include!("anvil_io.rs");
include!("recipes_and_stats.rs");
#[cfg(feature = "create-offload")]
include!("mod_offload_bridge.rs");
#[cfg(feature = "create-offload")]
include!("aeronautics_and_dim_bridge.rs");
include!("live_mod_bridge.rs");

// ---------- clients (multiplayer visibility) ----------
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

/// A block position. It is the key of every world registry below.
type BlockPos = (i32, i32, i32);
/// Lift a world position into the Overworld's dimension.
fn ow_block(pos: BlockPos) -> DimBlockPos {
    (DimId::Overworld, pos.0, pos.1, pos.2)
}
/// A chunk column, for the maps keyed by chunk rather than by block.
#[cfg(test)]
type ChunkKey = (i32, i32);
/// A chunk column *in a named dimension*: the key of the `WORLD` block overlay.
///
/// An overlay entry is an edit that replaced terrain, so it belongs to the dimension whose
/// generator that cell would otherwise have come from. Keyed by chunk alone, a Nether edit and
/// an Overworld edit of the same `(cx, cz)` are one entry, and whichever was written last answers
/// for both — the Nether's netherrack appears in the Overworld, or the edit is simply lost.
type DimChunkKey = (DimId, i32, i32);
/// A block position *in a named dimension*: the key of the block-scoped registries below. Keyed by
/// `(x, y, z)` alone, a Nether edit and an Overworld edit at the same coordinates are one entry.
type DimBlockPos = (DimId, i32, i32, i32);
/// The shape every world registry in this file has: one map behind one mutex,
/// built the first time something asks for it.
type Registry<K, V> = OnceLock<Mutex<HashMap<K, V>>>;
/// What those accessors hand out.
type LockedMap<K, V> = Mutex<HashMap<K, V>>;
/// One stored sign in a named dimension: its four lines, front or back.
type SignKey = (DimId, i32, i32, i32, u8);
type SignLines = [String; 4];
/// A block entity on its way out inside a chunk packet: position, type, NBT.
type ChunkBlockEntity = (BlockPos, i64, Vec<u8>);
/// An entity spawn queued for the clients that just became able to see it.
type SpawnJob = (
    i32,
    f64,
    f64,
    f64,
    Box<dyn Fn() -> Vec<(i64, Vec<u8>)> + Send>,
);
/// A throwable's hit, carried out of the borrow before it is applied:
/// (eid, mob hit, dx, dz, egg, x, y, z).
type ThrowHit = (i32, Option<i32>, f64, f64, bool, f64, f64, f64);
/// The advancement tree: player -> advancement -> finished criteria.
type Advancements = HashMap<String, HashMap<String, HashSet<String>>>;
/// The chunks a client has loaded, as (hot, warm) - the region snapshot.
#[cfg(test)]
type RegionSets = (HashSet<ChunkKey>, HashSet<ChunkKey>);


#[derive(Clone, Copy, Debug, PartialEq)]
struct EntityAabb {
    min_x: f64,
    min_y: f64,
    min_z: f64,
    max_x: f64,
    max_y: f64,
    max_z: f64,
}
impl EntityAabb {
    /// 1.21.1 `EntityDimensions.makeBoundingBox(x, y, z)` (`bsu.a(DDD)`):
    /// half-width is `(width / 2.0f32) as f64`, vertical span is `[y, y + height as f64]`.
    fn from_dimensions(x: f64, y: f64, z: f64, width: f32, height: f32) -> Self {
        let half_w = f64::from(width / 2.0);
        let h = f64::from(height);
        Self {
            min_x: x - half_w,
            min_y: y,
            min_z: z - half_w,
            max_x: x + half_w,
            max_y: y + h,
            max_z: z + half_w,
        }
    }
    /// 1.21.1 `AABB.inflate(x, y, z)` (`ewx.c(DDD)`).
    fn inflate(self, dx: f64, dy: f64, dz: f64) -> Self {
        Self {
            min_x: self.min_x - dx,
            min_y: self.min_y - dy,
            min_z: self.min_z - dz,
            max_x: self.max_x + dx,
            max_y: self.max_y + dy,
            max_z: self.max_z + dz,
        }
    }
    /// 1.21.1 `AABB.intersects(AABB)` (`ewx.c(Lewx;)Z` -> `ewx.a(DDDDDD)Z`).
    fn intersects(self, other: Self) -> bool {
        self.min_x < other.max_x
            && self.max_x > other.min_x
            && self.min_y < other.max_y
            && self.max_y > other.min_y
            && self.min_z < other.max_z
            && self.max_z > other.min_z
    }
}
// Nominal bounds on rate-driven retainers (audit: no global caps). The
// world overlay stays unbounded — it IS the save. These bound only the
// flood vectors: a hostile spawn bug could accrete mobs across nights,
// drops/xps can out-spawn their despawn window, hopper tracking grows
// with placed hoppers.
const MOB_CAP: usize = 4096;
const DROP_CAP: usize = 8192;
const XP_CAP: usize = 4096;
const HOPPER_CAP: usize = 65536;
// A generated hanging entity is static, so nothing turns one back into a spawn:
// the cap is what bounds a placement bug that stages the same marker in every
// chunk of a structure.
const FRAME_CAP: usize = 4096;


fn mob_snap_y(m: &Arc<Mob>, mp: &mut (f64, f64, f64, f32, f32)) -> Option<((i32, i32, i32), f32)> {
    // A mob in water or lava has its vertical motion owned by `LivingEntity.travel`
    // (`mob_fluid_travel_tick`), so the surface snap must not teleport it to the bed.
    if mob_has_support(m.etype, mp.0, mp.1, mp.2)
        || mob_in_water(m.etype, mp.0, mp.1, mp.2)
        || mob_in_lava(m.etype, mp.0, mp.1, mp.2)
    {
        return None;
    }
    // snap to ground surface; while buried only allow snaps within step height
    // so a fully enclosed mob cannot teleport out through solid blocks
    let (land_x, land_z) = (mp.0.floor() as i32, mp.2.floor() as i32);
    let mut sy = surface_y(land_x, land_z);
    let mut landed_in_water = false;
    while sy > MIN_Y as f64 {
        let below = block_at(land_x, sy as i32 - 1, land_z);
        if !fluid_blocks_motion(below)
            && entity_fluid_state_of(below).is_some_and(|fs| fs.kind == EntityFluidKind::Water)
        {
            landed_in_water = true;
            sy -= 1.0;
        } else {
            break;
        }
    }
    if (4286..=4293).contains(&block_at(land_x, sy as i32 - 1, land_z)) {
        sy -= 1.0 / 16.0;
    }
    if !mob_cell_solid(mp.0, mp.1, mp.2) || sy - mp.1 <= 1.2 {
        // refuse snaps that phase through solid blocks (e.g. sealed under a cap)
        let (fx, fz) = (land_x, land_z);
        let mut yy = mp.1.floor() as i32 + 1;
        while yy < sy.floor() as i32 {
            if !mob_passable(block_at(fx, yy, fz)) {
                return None;
            }
            yy += 1;
        }
        landed_in_water |= mob_in_water(m.etype, mp.0, sy, mp.2);
        let landing = (!landed_in_water).then_some((
            (land_x, (sy - 0.2).floor() as i32, land_z),
            (mp.1 - sy) as f32,
        ));
        // fall damage on descent >3 blocks; chicken/cat/ocelot immune (vanilla); water resets fall distance
        if !landed_in_water
            && mp.1 - sy > 3.0
            && !matches!(m.etype, 19 | 15 | 72)
            && GR_FALL_DAMAGE.load(Ordering::Relaxed) != 0
        {
            let mut hp = m.hp.lock().unwrap();
            *hp -= (mp.1 - sy - 3.0).floor() as f32;
            drop(hp);
            broadcast_seen(m.eid, |_c| (0x24, hurt_anim(m.eid)));
        }
        mp.1 = sy;
        return landing;
    }
    None
}
// ---------- mobs ----------
struct Mob {
    dim: DimId,
    eid: i32,
    uuid: [u8; 16],
    etype: i64,
    pos: Mutex<(f64, f64, f64, f32, f32)>,
    hp: Mutex<f32>,
    last_hit: Mutex<Instant>,
    hurt_at: Mutex<Instant>,
    /// `LivingEntity.invulnerableTime` (`btn.am:I`), tick-stamped: the tick of the last
    /// accepted hurt, so `LivingEntity.hurt`'s 10-tick window (`btn.a(brk,F)`) can be
    /// applied. Not persisted — vanilla does not keep the window across a reload either.
    last_hurt_tick: Mutex<Option<u64>>,
    fuse: Mutex<i32>,
    /// `Entity.remainingFireTicks`, separate from creeper/TNT fuse timing.
    fire_ticks: Mutex<i32>,
    love: Mutex<i32>,
    air: Mutex<i32>,
    /// `LivingEntity.deltaMovement` as far as the fluid branches of `LivingEntity.travel`
    /// maintain it (`src/entity_fluid.rs`). Land movement in this server is displacement based
    /// and does not read it, so it is cleared whenever the mob is in neither fluid — the
    /// non-fluid branch's friction does the same thing in vanilla.
    vel: Mutex<[f64; 3]>,
}
static MOBS: OnceLock<Mutex<Vec<Arc<Mob>>>> = OnceLock::new();
fn mobs() -> &'static Mutex<Vec<Arc<Mob>>> {
    MOBS.get_or_init(|| Mutex::new(Vec::new()))
}
static SHEARED: OnceLock<Mutex<std::collections::HashSet<i32>>> = OnceLock::new();
fn sheared() -> &'static Mutex<std::collections::HashSet<i32>> {
    SHEARED.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
static SITTING_CATS: OnceLock<Mutex<std::collections::HashSet<i32>>> = OnceLock::new();
fn sitting_cats() -> &'static Mutex<std::collections::HashSet<i32>> {
    SITTING_CATS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
fn surface_y(x: i32, z: i32) -> f64 {
    // top non-air block + 1 (flat world => 16 unless overlay edits)
    if worldgen::enabled() {
        return worldgen::chunk_cols(x >> 4, z >> 4)
            .map(|c| c.surface((x & 15) as usize, (z & 15) as usize) as f64 + 1.0)
            .unwrap_or(16.0);
    }
    let mut y = 100i32;
    while y > MIN_Y && block_at(x, y - 1, z) == 0 {
        y -= 1;
    }
    y as f64
}
// One owner for simulation cadence. Every per-tick increment, wire
// conversion and wall-clock boundary composes from these values, so a coherent
// cadence change is one edit instead of scattered multipliers.
// The world clock must still advance 20 game ticks per real second, so the
// per-tick advance is derived, not written as a literal 5.
const TICK_MS: u64 = 50;
const TICKS_PER_SECOND: u32 = 1000 / TICK_MS as u32;
/// Four seconds in server ticks, derived so it stays four seconds.
pub(crate) const MAX_DELAY_TICKS: usize = (4 * TICKS_PER_SECOND) as usize;
const GAME_TICKS_PER_TICK: u32 = 20 / TICKS_PER_SECOND;
/// Vanilla states every timing quantity in game ticks. Dividing by the ticks
/// per server tick converts one to the other, so a duration or interval keeps
/// the same real time at any cadence. Anything below one server tick collapses
/// to every tick rather than to zero, which would divide by zero in a modulo.
const fn server_ticks(game_ticks: u64) -> u64 {
    let ticks = game_ticks / GAME_TICKS_PER_TICK as u64;
    if ticks == 0 {
        1
    } else {
        ticks
    }
}
/// The lazy-schedule ladder doubles the re-check interval up to 80 game ticks
/// (4 s). Vanilla has no analogue; this is the lightweight schedule itself.
const fn backoff_ticks(fails: u64) -> u64 {
    server_ticks(10 << fails.saturating_sub(1))
}
/// Crop growth is a per-tick chance, not an interval, so the denominator scales
/// WITH ticks per second: the chance per real second stays 1/30 at any cadence.
/// This is the opposite direction from server_ticks, which divides.
const CROP_GROWTH_DENOMINATOR: u64 = 30 * TICKS_PER_SECOND as u64;
// Recurring schedules and one-shot lives that were tick literals. Naming the
// vanilla game-tick value lets the cutover derive the server-tick form from the
// single cadence owner instead of hunting for scattered numbers.
const AUTOSAVE_GT: u64 = 600; // 30s
static AUTOSAVE_ENABLED: AtomicBool = AtomicBool::new(true);
static DEFAULT_GAME_MODE: AtomicU8 = AtomicU8::new(0);
const SPAWN_CYCLE_GT: u64 = 200; // 10s
const CREEPER_FUSE_GT: u64 = 100; // 5s
const BOBBER_PACKET_GT: u64 = 15; // 0.75s
const BOLT_LIFE_GT: u64 = 10; // 0.5s visual
const TNT_FUSE_GT: u64 = 80; // 4s, TntBlock fuse
const AIR_SUPPLY_GT: u64 = 300; // 15s, entity air supply
const PICKUP_DELAY_GT: u64 = 10; // 0.5s, ItemEntity pickupDelay
const ARROW_TTL_GT: u64 = 1200; // 1200 ticks in ground; vanilla has no flying TTL
const MOB_ARROW_TTL_GT: u64 = 1200; // same AbstractArrow in-ground lifetime
const PEARL_TTL_GT: u64 = 250; // 12.5s
const SNOWBALL_TTL_GT: u64 = 300; // 15s
                                  // Mob counters that were bare tick counts at 4TPS, so the 20TPS cutover
                                  // shortened them fivefold. Stated as the real time they had before.
const BOBBER_BITE_GT: u64 = 75; // 3.75s, bobber settles before biting
const MOB_BURN_TICK_GT: u64 = 20; // 1s, vanilla burning damage interval
const CREEPER_IGNITE_GT: u64 = 30; // 1.5s, creeper fuse before igniting
#[cfg(test)]
const STARVE_GT: u64 = 80; // 4s, starvation damage interval
const FLUID_TICK_GT: u64 = 30; // 1.5s, scheduled lava/fluid tick
                               // Per-tick probabilities, not intervals: the denominator scales WITH ticks per
                               // second so the chance per real second is unchanged. This is the opposite
                               // direction from server_ticks, which divides.
const RAIN_CHANCE_DENOMINATOR: u64 = 6000 * TICKS_PER_SECOND as u64 / 4;
const MOB_SPAWN_CHANCE_DENOMINATOR: u64 = 400 * TICKS_PER_SECOND as u64 / 4;
const DROP_LIFE_GT: u64 = 6000; // 300s, ItemEntity lifespan
const XP_LIFE_GT: u64 = 6000; // 300s, ExperienceOrb lifespan
/// chain
const fn chain_fuse_left(jitter: u64) -> u64 {
    server_ticks(10 + if jitter > 20 { 20 } else { jitter })
}
// Wall-clock owners. Vanilla defines these in real time, not in game ticks, so
// they anchor on Instants: changing the simulation cadence must not shift them.
const TIME_NOTIFY_MS: u64 = 1_000; // day/night display sync
const SIZE_LOG_MS: u64 = 5_000; // container-count watch (diagnostic)
static TIME_NOTIFY_AT: OnceLock<Mutex<Instant>> = OnceLock::new();
static SIZE_LOG_AT: OnceLock<Mutex<Instant>> = OnceLock::new();
/// The tick loop's pacing rule, extracted so it can be held to a contract.
/// Returns the next deadline given the current one and the work that just ran.
/// `overrun` re-anchors instead of catching up, so an overloaded server cannot
/// accumulate an unbounded backlog of deadlines.
fn next_deadline(deadline: Instant, overrun: bool) -> Instant {
    if overrun {
        Instant::now()
    } else {
        deadline
    }
}
fn wall_anchor(cell: &'static OnceLock<Mutex<Instant>>) -> &'static Mutex<Instant> {
    cell.get_or_init(|| Mutex::new(Instant::now()))
}
/// Run `body` at most once per `period` of wall-clock time. The anchor is the
/// previous firing, so neither cadence nor tick position can shift the interval.
fn every_ms(cell: &'static OnceLock<Mutex<Instant>>, period: u64, body: impl FnOnce()) {
    let fire = {
        let mut last = wall_anchor(cell).lock().unwrap();
        if last.elapsed() >= Duration::from_millis(period) {
            *last = Instant::now();
            true
        } else {
            false
        }
    };
    if fire {
        body();
    }
}
/// Cross-platform OS entropy fill. Unix reads `/dev/urandom`; Windows calls
/// Win32 `BCryptGenRandom` with the system-preferred RNG — zero extra crates.
/// Returns false when the platform source could not supply the bytes.
fn os_fill_random(buf: &mut [u8]) -> bool {
    #[cfg(unix)]
    return std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .is_ok();
    #[cfg(windows)]
    {
        #[link(name = "bcrypt")]
        extern "system" {
            fn BCryptGenRandom(
                halgorithm: *mut core::ffi::c_void,
                pbbuffer: *mut u8,
                cbbuffer: u32,
                dwflags: u32,
            ) -> i32;
        }
        const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;
        // NTSTATUS success codes are non-negative; a NULL algorithm handle
        // selects the system-preferred RNG when the flag is set.
        unsafe {
            BCryptGenRandom(
                core::ptr::null_mut(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            ) >= 0
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = buf;
        false
    }
}
static TICK: AtomicU64 = AtomicU64::new(0);
static SPAWN_ATTEMPT: AtomicU64 = AtomicU64::new(0);
static WORLD_TIME: AtomicU64 = AtomicU64::new(1000); // morning
static BOLTS: OnceLock<Mutex<Vec<(i32, i32)>>> = OnceLock::new(); // (eid, ticks-left) transient lightning visuals
fn bolts() -> &'static Mutex<Vec<(i32, i32)>> {
    BOLTS.get_or_init(|| Mutex::new(Vec::new()))
}
static RAINING: AtomicU64 = AtomicU64::new(0); // ticks of rain remaining
static THUNDERING: AtomicU64 = AtomicU64::new(0); // ticks of thunder (subset of rain)
                                                  // vanilla gamerules wired to systems that exist (0=false, 1=true);
                                                  // persisted in the meta tail — they are world state, not session.
static GR_KEEP_INVENTORY: AtomicU8 = AtomicU8::new(0);
static GR_DAYLIGHT_CYCLE: AtomicU8 = AtomicU8::new(1);
static GR_WEATHER_CYCLE: AtomicU8 = AtomicU8::new(1);
static GR_MOB_SPAWNING: AtomicU8 = AtomicU8::new(1);
static GR_MOB_GRIEFING: AtomicU8 = AtomicU8::new(1);
static GR_NATURAL_REGEN: AtomicU8 = AtomicU8::new(1);
static GR_MOB_LOOT: AtomicU8 = AtomicU8::new(1);
static GR_MAX_ENTITY_CRAMMING: AtomicI32 = AtomicI32::new(ENTITY_CRAMMING_DEFAULT);
// P1-A: the next tranche of vanilla gamerules with live consumers. The first
// six gate the random-tick and drop systems; the damage/death set gates the
// environmental damage arms in this file and the death announcements; the rest
// are stored atomics whose consumers land with the systems they govern.
static GR_RANDOM_TICK_SPEED: AtomicI32 = AtomicI32::new(3);
static GR_DO_FIRE_TICK: AtomicU8 = AtomicU8::new(1);
static GR_SNOW_ACCUMULATION_HEIGHT: AtomicI32 = AtomicI32::new(1);
static GR_DO_TILE_DROPS: AtomicU8 = AtomicU8::new(1);
static GR_DO_ENTITY_DROPS: AtomicU8 = AtomicU8::new(1);
static GR_FALL_DAMAGE: AtomicU8 = AtomicU8::new(1);
static GR_DROWNING_DAMAGE: AtomicU8 = AtomicU8::new(1);
static GR_FIRE_DAMAGE: AtomicU8 = AtomicU8::new(1);
static GR_FREEZE_DAMAGE: AtomicU8 = AtomicU8::new(1);
static GR_DO_IMMEDIATE_RESPAWN: AtomicU8 = AtomicU8::new(0);
static GR_SHOW_DEATH_MESSAGES: AtomicU8 = AtomicU8::new(1);
static GR_ANNOUNCE_ADVANCEMENTS: AtomicU8 = AtomicU8::new(1);
static GR_DISABLE_RAIDS: AtomicU8 = AtomicU8::new(0);
static GR_PLAYERS_SLEEPING_PERCENTAGE: AtomicI32 = AtomicI32::new(100);
static GR_WATER_SOURCE_CONVERSION: AtomicU8 = AtomicU8::new(1);
static GR_LAVA_SOURCE_CONVERSION: AtomicU8 = AtomicU8::new(0);
static GR_SEND_COMMAND_FEEDBACK: AtomicU8 = AtomicU8::new(1);
static GR_COMMAND_MODIFICATION_BLOCK_LIMIT: AtomicI32 = AtomicI32::new(32768);
static GR_DO_VINES_SPREAD: AtomicU8 = AtomicU8::new(1);

/// The gamerules with a live runtime atomic, as one name -> atomic mapping.
/// `/gamerule` (set, query and list), the save footer and the load path all
/// resolve through these two, so a rule can never be stored but unread, or
/// read but unrestorable.
fn gamerule_atomic_value(name: &str) -> Option<i32> {
    Some(match name {
        "keepInventory" => GR_KEEP_INVENTORY.load(Ordering::Relaxed) as i32,
        "doDaylightCycle" => GR_DAYLIGHT_CYCLE.load(Ordering::Relaxed) as i32,
        "doWeatherCycle" => GR_WEATHER_CYCLE.load(Ordering::Relaxed) as i32,
        "doMobSpawning" => GR_MOB_SPAWNING.load(Ordering::Relaxed) as i32,
        "mobGriefing" => GR_MOB_GRIEFING.load(Ordering::Relaxed) as i32,
        "naturalRegeneration" => GR_NATURAL_REGEN.load(Ordering::Relaxed) as i32,
        "doMobLoot" => GR_MOB_LOOT.load(Ordering::Relaxed) as i32,
        "maxEntityCramming" => GR_MAX_ENTITY_CRAMMING.load(Ordering::Relaxed),
        "randomTickSpeed" => GR_RANDOM_TICK_SPEED.load(Ordering::Relaxed),
        "doFireTick" => GR_DO_FIRE_TICK.load(Ordering::Relaxed) as i32,
        "snowAccumulationHeight" => GR_SNOW_ACCUMULATION_HEIGHT.load(Ordering::Relaxed),
        "doTileDrops" => GR_DO_TILE_DROPS.load(Ordering::Relaxed) as i32,
        "doEntityDrops" => GR_DO_ENTITY_DROPS.load(Ordering::Relaxed) as i32,
        "fallDamage" => GR_FALL_DAMAGE.load(Ordering::Relaxed) as i32,
        "drowningDamage" => GR_DROWNING_DAMAGE.load(Ordering::Relaxed) as i32,
        "fireDamage" => GR_FIRE_DAMAGE.load(Ordering::Relaxed) as i32,
        "freezeDamage" => GR_FREEZE_DAMAGE.load(Ordering::Relaxed) as i32,
        "doImmediateRespawn" => GR_DO_IMMEDIATE_RESPAWN.load(Ordering::Relaxed) as i32,
        "showDeathMessages" => GR_SHOW_DEATH_MESSAGES.load(Ordering::Relaxed) as i32,
        "announceAdvancements" => GR_ANNOUNCE_ADVANCEMENTS.load(Ordering::Relaxed) as i32,
        "disableRaids" => GR_DISABLE_RAIDS.load(Ordering::Relaxed) as i32,
        "playersSleepingPercentage" => {
            GR_PLAYERS_SLEEPING_PERCENTAGE.load(Ordering::Relaxed)
        }
        "waterSourceConversion" => GR_WATER_SOURCE_CONVERSION.load(Ordering::Relaxed) as i32,
        "lavaSourceConversion" => GR_LAVA_SOURCE_CONVERSION.load(Ordering::Relaxed) as i32,
        "sendCommandFeedback" => GR_SEND_COMMAND_FEEDBACK.load(Ordering::Relaxed) as i32,
        "commandModificationBlockLimit" => {
            GR_COMMAND_MODIFICATION_BLOCK_LIMIT.load(Ordering::Relaxed)
        }
        "doVinesSpread" => GR_DO_VINES_SPREAD.load(Ordering::Relaxed) as i32,
        _ => return None,
    })
}
fn gamerule_atomic_store(name: &str, value: i32) -> bool {
    match name {
        "keepInventory" => GR_KEEP_INVENTORY.store(value as u8, Ordering::Relaxed),
        "doDaylightCycle" => GR_DAYLIGHT_CYCLE.store(value as u8, Ordering::Relaxed),
        "doWeatherCycle" => GR_WEATHER_CYCLE.store(value as u8, Ordering::Relaxed),
        "doMobSpawning" => GR_MOB_SPAWNING.store(value as u8, Ordering::Relaxed),
        "mobGriefing" => GR_MOB_GRIEFING.store(value as u8, Ordering::Relaxed),
        "naturalRegeneration" => GR_NATURAL_REGEN.store(value as u8, Ordering::Relaxed),
        "doMobLoot" => GR_MOB_LOOT.store(value as u8, Ordering::Relaxed),
        "maxEntityCramming" => GR_MAX_ENTITY_CRAMMING.store(value, Ordering::Relaxed),
        "randomTickSpeed" => GR_RANDOM_TICK_SPEED.store(value, Ordering::Relaxed),
        "doFireTick" => GR_DO_FIRE_TICK.store(value as u8, Ordering::Relaxed),
        "snowAccumulationHeight" => {
            GR_SNOW_ACCUMULATION_HEIGHT.store(value, Ordering::Relaxed)
        }
        "doTileDrops" => GR_DO_TILE_DROPS.store(value as u8, Ordering::Relaxed),
        "doEntityDrops" => GR_DO_ENTITY_DROPS.store(value as u8, Ordering::Relaxed),
        "fallDamage" => GR_FALL_DAMAGE.store(value as u8, Ordering::Relaxed),
        "drowningDamage" => GR_DROWNING_DAMAGE.store(value as u8, Ordering::Relaxed),
        "fireDamage" => GR_FIRE_DAMAGE.store(value as u8, Ordering::Relaxed),
        "freezeDamage" => GR_FREEZE_DAMAGE.store(value as u8, Ordering::Relaxed),
        "doImmediateRespawn" => GR_DO_IMMEDIATE_RESPAWN.store(value as u8, Ordering::Relaxed),
        "showDeathMessages" => GR_SHOW_DEATH_MESSAGES.store(value as u8, Ordering::Relaxed),
        "announceAdvancements" => {
            GR_ANNOUNCE_ADVANCEMENTS.store(value as u8, Ordering::Relaxed)
        }
        "disableRaids" => GR_DISABLE_RAIDS.store(value as u8, Ordering::Relaxed),
        "playersSleepingPercentage" => {
            GR_PLAYERS_SLEEPING_PERCENTAGE.store(value, Ordering::Relaxed)
        }
        "waterSourceConversion" => {
            GR_WATER_SOURCE_CONVERSION.store(value as u8, Ordering::Relaxed)
        }
        "lavaSourceConversion" => {
            GR_LAVA_SOURCE_CONVERSION.store(value as u8, Ordering::Relaxed)
        }
        "sendCommandFeedback" => {
            GR_SEND_COMMAND_FEEDBACK.store(value as u8, Ordering::Relaxed)
        }
        "commandModificationBlockLimit" => {
            GR_COMMAND_MODIFICATION_BLOCK_LIMIT.store(value, Ordering::Relaxed)
        }
        "doVinesSpread" => GR_DO_VINES_SPREAD.store(value as u8, Ordering::Relaxed),
        _ => return false,
    }
    true
}

/// `randomTickSpeed`, clamped to the range a pass can honor: `0` disables
/// random ticks; the upper bound keeps one tick from sampling a section more
/// than once per cell on average.
fn gr_random_tick_speed() -> u32 {
    (GR_RANDOM_TICK_SPEED.load(Ordering::Relaxed)).clamp(0, 4096) as u32
}
/// `snowAccumulationHeight`: `0` disables accumulation; the snow block's
/// `layers` property tops out at 8.
fn gr_snow_accumulation_height() -> u8 {
    (GR_SNOW_ACCUMULATION_HEIGHT.load(Ordering::Relaxed)).clamp(0, 8) as u8
}
/// Ready for the raid lane (P1-B): raids do not start while the rule is set.
#[allow(dead_code)]
fn gr_disable_raids() -> bool {
    GR_DISABLE_RAIDS.load(Ordering::Relaxed) != 0
}
/// Ready for the sleep lane (P1-C): percent of players required to skip night.
#[allow(dead_code)]
fn gr_players_sleeping_percentage() -> i32 {
    GR_PLAYERS_SLEEPING_PERCENTAGE.load(Ordering::Relaxed)
}
/// Ready for the freeze lane: powder-snow damage has no damage site yet, so
/// the rule is stored and synced but has nothing to gate.
#[allow(dead_code)]
fn gr_freeze_damage() -> bool {
    GR_FREEZE_DAMAGE.load(Ordering::Relaxed) != 0
}
/// `doEntityDrops`: non-mob entity drops — broken minecarts and falling blocks
/// that leave the world.
fn gr_do_entity_drops() -> bool {
    GR_DO_ENTITY_DROPS.load(Ordering::Relaxed) != 0
}
/// Ready for the vine lane: vines have no random tick yet.
#[allow(dead_code)]
fn gr_do_vines_spread() -> bool {
    GR_DO_VINES_SPREAD.load(Ordering::Relaxed) != 0
}
/// `doMobSpawning`: the gamerule that gates the pearl-landing endermite roll —
/// the same atomic the natural-spawn passes read.
fn gr_do_mob_spawning() -> bool {
    GR_MOB_SPAWNING.load(Ordering::Relaxed) == 1
}
/// `FlowableFluid.canConvertToSource` per kind, from the two conversion rules.
fn source_conversion_enabled(kind: flowing_fluid::Kind) -> bool {
    match kind {
        flowing_fluid::Kind::Water => {
            GR_WATER_SOURCE_CONVERSION.load(Ordering::Relaxed) != 0
        }
        flowing_fluid::Kind::Lava => GR_LAVA_SOURCE_CONVERSION.load(Ordering::Relaxed) != 0,
    }
}
/// `dimension_type.ultrawarm`: water cannot exist and ice sublimates to air.
fn dim_ultrawarm(dim: DimId) -> bool {
    dim == DimId::Nether || worldgen::dimension_spec(dim).is_some_and(|spec| spec.ultrawarm)
}
// world spawn (vanilla /setworldspawn) + difficulty — both persisted
static WORLD_SPAWN: Mutex<(f64, f64, f64, f32)> = Mutex::new((0.5, 16.0, 0.5, 0.0));
// 0=peaceful 1=easy 2=normal 3=hard; gates hostile night spawns
static DIFFICULTY: AtomicU8 = AtomicU8::new(2);
static POWERED_PLATES: OnceLock<Mutex<HashSet<(i32, i32, i32)>>> = OnceLock::new();
fn powered_plates() -> &'static Mutex<HashSet<(i32, i32, i32)>> {
    POWERED_PLATES.get_or_init(|| Mutex::new(HashSet::new()))
}
// plate states: unpowered id is odd, powered = id-1 (vanilla bool order true,false)
fn plate_unpowered(st: u32) -> bool {
    st == 5651 || (5717..=5733).contains(&st) && st % 2 == 1
}
fn plate_powered(st: u32) -> bool {
    st == 5650 || (5716..=5732).contains(&st) && st.is_multiple_of(2)
}

fn environment_time_tick() {
    // One region borrow for the stage. A static scan confirmed that nothing this
    // stage calls re-enters the region lock.
    //
    // Split under `MC_SPINE_TICK_PROFILE=1`: this step is the whole tick body under a
    // moving viewer (9-11 ms of 9.3-11.4), so the split below says which part of it.
    let __prof = tickprof::enabled();
    let mut __t = __prof.then(std::time::Instant::now);
    let region = region_guard();
    if let Some(t) = __t {
        tickprof::add("env/region_guard", t.elapsed());
    }
    __t = __prof.then(std::time::Instant::now);
    let t = TICK.fetch_add(1, Ordering::Relaxed);
    world_border().lock().unwrap().finish_lerp(border_now_ms());
    {
        if let Some(t) = __t {
            tickprof::add("env/clock", t.elapsed());
        }
        __t = __prof.then(std::time::Instant::now);
    }
    // world clock advances every tick (20 tps => +20/s)
    let wt = if GR_DAYLIGHT_CYCLE.load(Ordering::Relaxed) == 1 {
        WORLD_TIME.fetch_add(GAME_TICKS_PER_TICK as u64, Ordering::Relaxed) % 24000
    } else {
        WORLD_TIME.load(Ordering::Relaxed) % 24000
    };
    every_ms(&TIME_NOTIFY_AT, TIME_NOTIFY_MS, || {
        broadcast(0, |_c| {
            let mut p = Vec::new();
            p.extend_from_slice(&t.to_be_bytes()); // world age (i64)
            p.extend_from_slice(&(wt as i64).to_be_bytes());
            (0x64, p)
        });
    });
    every_ms(&SIZE_LOG_AT, SIZE_LOG_MS, || {
        #[cfg(target_os = "linux")]
        let rss = std::fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .find(|l| l.starts_with("VmRSS"))
            .map(|l| l.to_string())
            .unwrap_or_default();
        #[cfg(not(target_os = "linux"))]
        let rss = String::new();
        eprintln!("[sz] t={} {} | mobs={} drops={} xps={} arrows={} bob={} pearl={} primed={} chests={} furn={} crops={} world={} inv={} clients={}",
            t, rss.trim(),
            mobs().lock().unwrap().len(), drops().lock().unwrap().len(), xps().lock().unwrap().len(),
            arrows().lock().unwrap().len(), bobbers().lock().unwrap().len(), pearls().lock().unwrap().len(),
            primed().lock().unwrap().len(), chests().lock().unwrap().len(), furnaces().lock().unwrap().len(),
            crops().lock().unwrap().len(), world().lock().unwrap().len(), inv_store().lock().unwrap().len(),
            clients().lock().unwrap().len());
    });
    // weather: occasional rain
    {
        let rt = RAINING.load(Ordering::Relaxed);
        if rt > 0 {
            THUNDERING
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_sub(1))
                })
                .ok();
            if GR_WEATHER_CYCLE.load(Ordering::Relaxed) == 1
                && RAINING.fetch_sub(1, Ordering::Relaxed) == 1
            {
                THUNDERING.store(0, Ordering::Relaxed);
                broadcast(-1, |_c| (0x22, vec![2u8, 0, 0, 0, 0]));
                broadcast(-1, |_c| {
                    let mut p = vec![8u8];
                    p.extend_from_slice(&0f32.to_be_bytes());
                    (0x22, p.clone())
                });
            }
        } else if GR_WEATHER_CYCLE.load(Ordering::Relaxed) == 1
            && t.wrapping_mul(2654435761)
                .is_multiple_of(RAIN_CHANCE_DENOMINATOR)
        {
            RAINING.store(server_ticks(12000), Ordering::Relaxed);
            broadcast(-1, |_c| (0x22, vec![1u8, 0, 0, 0, 0]));
            if world_rng::next_u64().is_multiple_of(8) {
                // ~1/8 of rainstorms are thunderstorms (vanilla-ish)
                THUNDERING.store(server_ticks(3600), Ordering::Relaxed);
                broadcast(-1, |_c| {
                    let mut p = vec![8u8];
                    p.extend_from_slice(&1f32.to_be_bytes());
                    (0x22, p.clone())
                });
            }
        }
    }
    {
        if let Some(t) = __t {
            tickprof::add("env/weather+lightning", t.elapsed());
        }
        __t = __prof.then(std::time::Instant::now);
    }
    // transient lightning bolts: despawn after ~0.5s
    {
        let mut bs = bolts().lock().unwrap();
        let mut i = 0;
        while i < bs.len() {
            bs[i].1 -= 1;
            if bs[i].1 <= 0 {
                let (be, _) = bs.remove(i);
                broadcast_seen_destroy(be);
            } else {
                i += 1;
            }
        }
    }
    // Player hunger and environmental damage.
    {
        let cls: Vec<Arc<Client>> = client_snapshot();
        for c in cls.iter() {
            let gm = *c.gm.lock().unwrap();
            if matches!(gm, 1 | 3) || *c.hp.lock().unwrap() <= 0.0 {
                continue;
            }
            let (px, py, pz, ..) = {
                let p = c.pos.lock().unwrap();
                *p
            };
            // elytra fall-flying: airborne + descending + elytra chestplate
            {
                let (_x, _y, _z, _p, _q, gnd) = {
                    let p = c.pos.lock().unwrap();
                    *p
                };
                let ff = *c.fall_from.lock().unwrap();
                let ely = {
                    let iv = c.inv.lock().unwrap();
                    iv.get(&6i16).map(|v| v.0).unwrap_or(0)
                } == name_to_item("elytra").unwrap_or(-9);
                let glide = !gnd && ely && py < ff - 0.1 && *c.hp.lock().unwrap() > 0.0;
                // vanilla: elytra pays 1 durability per second of glide
                if glide && t.is_multiple_of(server_ticks(20)) && damage_inv_slot(c, 6) {
                    let st =
                        c.inv
                            .lock()
                            .unwrap()
                            .get(&6i16)
                            .cloned()
                            .unwrap_or((0, 0, Vec::new()));
                    resync_slot(c, 0, 6, &st);
                }
                let mut fl = c.fall_fly.lock().unwrap();
                if *fl != glide {
                    *fl = glide;
                    drop(fl);
                    let sneak = *c.sneaking.lock().unwrap();
                    let sprint = *c.sprinting.lock().unwrap();
                    let mut p = Vec::new();
                    write_varint(&mut p, c.eid as i64);
                    p.push(0);
                    p.push(0);
                    p.push(((sneak as u8) * 2) | ((sprint as u8) * 8) | ((glide as u8) * 0x80));
                    p.push(6);
                    write_varint(&mut p, 21);
                    write_varint(
                        &mut p,
                        if glide {
                            1
                        } else if sneak {
                            5
                        } else {
                            0
                        },
                    );
                    p.push(0xff);
                    broadcast_seen(c.eid, |_c| (0x58, p.clone()));
                }
            }
            // ServerPlayer decrements the damage cooldown on its own tick.
            {
                let mut invulnerable = c.invulnerable.lock().unwrap();
                *invulnerable = (*invulnerable - 1).max(0);
            }
            let exhaustion = player_movement_exhaustion(c, (px, py, pz));
            let mut h = c.hp.lock().unwrap();
            let mut dirty = tick_player_food(c, &mut h, max_hp_of(&c.name), exhaustion, t);
            if *h <= 0.0 {
                drop(h);
                totem_check(c);
                h = c.hp.lock().unwrap();
            }
            let player_dim = *c.dim.lock().unwrap();
            let sneaking = *c.sneaking.lock().unwrap();
            let fall_fly = *c.fall_fly.lock().unwrap();
            {
                // drowning: 1.21.1 Entity.updateFluidOnEyes + LivingEntity.baseTick (!bubble_column)
                let submerged = player_drowning_submerged(player_dim, px, py, pz, sneaking, fall_fly);
                let mut air = c.air.lock().unwrap();
                if !submerged {
                    *air = server_ticks(AIR_SUPPLY_GT) as i32;
                } else if *air > 0 {
                    // respiration: air drains (lvl+1)x slower
                    let resp: i64 = {
                        let iv = c.inv.lock().unwrap();
                        enchant_level(&iv.get(&5i16).map(|v| v.2.clone()).unwrap_or_default(), 30)
                    };
                    if resp == 0 || t.is_multiple_of((resp + 1) as u64) {
                        *air -= 1;
                    }
                } else if t.is_multiple_of(server_ticks(20))
                    && GR_DROWNING_DAMAGE.load(Ordering::Relaxed) != 0
                {
                    *h = (*h - 2.0).max(0.0);
                    dirty = true;
                    // vanilla: environmental damage arms the same timer, so a melee hit in
                    // the next ten ticks is measured against this amount
                    *c.last_hurt.lock().unwrap() = 2.0;
                    *c.invulnerable.lock().unwrap() = server_ticks(20) as i32;
                    if *h <= 0.0 {
                        drop(h);
                        totem_check(c);
                        h = c.hp.lock().unwrap();
                    }
                }
            }
            {
                // lava: 1.21.1 Entity.isInLava (fluidHeight[LAVA] > 0.0 with deflate(0.001))
                let fire_immune = c.effs.lock().unwrap().iter().any(|e| e.id == 11);
                let in_lava = !fire_immune
                    && GR_FIRE_DAMAGE.load(Ordering::Relaxed) != 0
                    && player_in_lava(player_dim, px, py, pz, sneaking, fall_fly);
                if in_lava && t.is_multiple_of(server_ticks(10)) {
                    *h = (*h - 4.0 * (1.0 - 0.04 * enchant_epf(c, 4) as f32)).max(0.0);
                    dirty = true;
                    // vanilla: environmental damage arms the same timer, so a melee hit in
                    // the next ten ticks is measured against this amount
                    *c.last_hurt.lock().unwrap() = 4.0 * (1.0 - 0.04 * enchant_epf(c, 4) as f32);
                    *c.invulnerable.lock().unwrap() = server_ticks(20) as i32;
                    if *h <= 0.0 {
                        drop(h);
                        totem_check(c);
                        h = c.hp.lock().unwrap();
                    }
                }
                // standing in fire: 1hp/s (vanilla), fire_resist immune,
                // fire_protection epf applies, armor does not
                let fst = name_to_state("fire").unwrap_or(u32::MAX);
                let in_fire = !fire_immune
                    && GR_FIRE_DAMAGE.load(Ordering::Relaxed) != 0
                    && (block_at_dim(player_dim, px.floor() as i32, py.floor() as i32, pz.floor() as i32) == fst);
                if in_fire && t.is_multiple_of(server_ticks(20)) {
                    *h = (*h - 1.0 * (1.0 - 0.04 * enchant_epf(c, 4) as f32)).max(0.0);
                    dirty = true;
                    // vanilla: environmental damage arms the same timer, so a melee hit in
                    // the next ten ticks is measured against this amount
                    *c.last_hurt.lock().unwrap() = 1.0 * (1.0 - 0.04 * enchant_epf(c, 4) as f32);
                    *c.invulnerable.lock().unwrap() = server_ticks(20) as i32;
                    if *h <= 0.0 {
                        drop(h);
                        totem_check(c);
                        h = c.hp.lock().unwrap();
                    }
                }
                // powder snow: vanilla LivingEntity freezes for 7s, then takes
                // 1hp per 2s. The ramp lives in a per-eid timer since players
                // carry no frozen field yet.
                {
                    let in_powder = name_to_state("powder_snow").is_some_and(|st| {
                        block_at_dim(
                            player_dim,
                            px.floor() as i32,
                            py.floor() as i32,
                            pz.floor() as i32,
                        ) == st
                    });
                    let frozen = {
                        let mut timers = w1_frozen_ticks().lock().unwrap_or_else(|e| e.into_inner());
                        if in_powder {
                            *timers.entry(c.eid).or_insert(0) += 1;
                            *timers.get(&c.eid).unwrap_or(&0)
                        } else {
                            timers.remove(&c.eid);
                            0
                        }
                    };
                    if frozen >= server_ticks(140) as u64
                        && t.is_multiple_of(server_ticks(40))
                        && GR_FREEZE_DAMAGE.load(Ordering::Relaxed) != 0
                    {
                        *h = (*h - 1.0).max(0.0);
                        dirty = true;
                        // vanilla: environmental damage arms the same timer, so a melee hit in
                        // the next ten ticks is measured against this amount
                        *c.last_hurt.lock().unwrap_or_else(|e| e.into_inner()) = 1.0;
                        *c.invulnerable.lock().unwrap_or_else(|e| e.into_inner()) = server_ticks(20) as i32;
                        if *h <= 0.0 {
                            drop(h);
                            totem_check(c);
                            h = c.hp.lock().unwrap_or_else(|e| e.into_inner());
                        }
                    }
                }
                // void: 4hp per 0.5s below y=-64, ignores armor (vanilla)
                if py < -64.0 && t.is_multiple_of(server_ticks(10)) {
                    *h = (*h - 4.0).max(0.0);
                    dirty = true;
                    // vanilla: environmental damage arms the same timer, so a melee hit in
                    // the next ten ticks is measured against this amount
                    *c.last_hurt.lock().unwrap() = 4.0;
                    *c.invulnerable.lock().unwrap() = server_ticks(20) as i32;
                    if *h <= 0.0 {
                        drop(h);
                        totem_check(c);
                        h = c.hp.lock().unwrap();
                    }
                }
                // suffocation: solid block at head height, 1hp/0.5s (vanilla)
                if t.is_multiple_of(server_ticks(10)) {
                    let hb = block_at(
                        px.floor() as i32,
                        (py + 1.62).floor() as i32,
                        pz.floor() as i32,
                    );
                    if hb != u32::MAX && !mob_passable(hb) {
                        *h = (*h - 1.0).max(0.0);
                        dirty = true;
                        // vanilla: environmental damage arms the same timer, so a melee hit in
                        // the next ten ticks is measured against this amount
                        *c.last_hurt.lock().unwrap() = 1.0;
                        *c.invulnerable.lock().unwrap() = server_ticks(20) as i32;
                        if *h <= 0.0 {
                            drop(h);
                            totem_check(c);
                            h = c.hp.lock().unwrap();
                        }
                    }
                }
                // contact damage: cactus adjacent (1hp/0.5s), magma underfoot
                // (1hp/0.5s, sneaking safe, fire kind), berry bush inside
                let (bx, bz) = (px.floor() as i32, pz.floor() as i32);
                if t.is_multiple_of(server_ticks(10)) {
                    let cst = name_to_state("cactus").unwrap_or(u32::MAX);
                    let near_cactus = t.is_multiple_of(server_ticks(20))
                        && [[1, 0], [-1, 0], [0, 1], [0, -1], [0, 0]]
                            .iter()
                            .any(|&[dx, dz]| block_at(bx + dx, py.floor() as i32, bz + dz) == cst);
                    if near_cactus {
                        *h = (*h - 1.0).max(0.0);
                        dirty = true;
                        // vanilla: environmental damage arms the same timer, so a melee hit in
                        // the next ten ticks is measured against this amount
                        *c.last_hurt.lock().unwrap() = 1.0;
                        *c.invulnerable.lock().unwrap() = server_ticks(20) as i32;
                        if *h <= 0.0 {
                            drop(h);
                            totem_check(c);
                            h = c.hp.lock().unwrap();
                        }
                    }
                    let mst = name_to_state("magma_block").unwrap_or(u32::MAX);
                    let on_magma = !fire_immune
                        && GR_FIRE_DAMAGE.load(Ordering::Relaxed) != 0
                        && !*c.sneaking.lock().unwrap()
                        && block_at(bx, (py - 0.5).floor() as i32, bz) == mst;
                    if on_magma {
                        *h = (*h - 1.0 * (1.0 - 0.04 * enchant_epf(c, 4) as f32)).max(0.0);
                        dirty = true;
                        // vanilla: environmental damage arms the same timer, so a melee hit in
                        // the next ten ticks is measured against this amount
                        *c.last_hurt.lock().unwrap() =
                            1.0 * (1.0 - 0.04 * enchant_epf(c, 4) as f32);
                        *c.invulnerable.lock().unwrap() = server_ticks(20) as i32;
                        if *h <= 0.0 {
                            drop(h);
                            totem_check(c);
                            h = c.hp.lock().unwrap();
                        }
                    }
                    let bst = name_to_state("sweet_berry_bush").unwrap_or(u32::MAX);
                    if t.is_multiple_of(server_ticks(20))
                        && block_at(bx, py.floor() as i32, bz) == bst
                    {
                        *h = (*h - 0.5).max(0.0);
                        dirty = true;
                        // vanilla: environmental damage arms the same timer, so a melee hit in
                        // the next ten ticks is measured against this amount
                        *c.last_hurt.lock().unwrap() = 0.5;
                        *c.invulnerable.lock().unwrap() = server_ticks(20) as i32;
                        if *h <= 0.0 {
                            drop(h);
                            totem_check(c);
                            h = c.hp.lock().unwrap();
                        }
                    }
                }
            }
            if dirty {
                let health = *h;
                drop(h);
                let packet = health_pkt_c(health, c);
                if let Ok(mut w) = c.sock.lock() {
                    let _ = send_packet(&mut w, 0x5d, packet);
                }
            }
        }
    }
    {
        if let Some(t) = __t {
            tickprof::add("env/hunger+plates", t.elapsed());
        }
        __t = __prof.then(std::time::Instant::now);
    }
    // pressure plates: an entity standing on one powers it; the plate
    // stays powered until the cell clears — vanilla bool order puts
    // powered at the even id, unpowered odd
    {
        let __tspots = __prof.then(std::time::Instant::now);
        let mut spots: Vec<(i32, i32, i32)> = Vec::new();
        {
            for c in clients().lock().unwrap().iter() {
                let p = c.pos.lock().unwrap();
                spots.push((p.0.floor() as i32, p.1.floor() as i32, p.2.floor() as i32));
            }
        }
        {
            for m in mobs().lock().unwrap().iter().filter(|mob| mob.dim == DimId::Overworld) {
                let p = m.pos.lock().unwrap();
                spots.push((p.0.floor() as i32, p.1.floor() as i32, p.2.floor() as i32));
            }
        }
        if let Some(t) = __tspots {
            tickprof::add("plates/spot_gather", t.elapsed());
        }
        let __tloop = __prof.then(std::time::Instant::now);
        let mut pw = powered_plates().lock().unwrap();
        if __prof {
            use std::sync::atomic::Ordering;
            tickprof::PLATE_SPOTS.fetch_add(spots.len() as u64, Ordering::Relaxed);
        }
        for (x, y, z) in spots.iter().copied() {
            // A pressure plate the server can power was placed by a player, so it is in
            // the overlay; a generated one would come from a structure, which this server
            // does not place yet. Reading the overlay only keeps this loop off the
            // worldgen path entirely.
            let st = overlay_block(x, y, z).unwrap_or(0);
            if plate_unpowered(st) {
                if __prof {
                    tickprof::PLATE_ON.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                set_block(x, y, z, st - 1);
                note_block_update(x, y, z, st - 1);
                redstone_update(x, y - 1, z, true); // plate powers the block below
                pw.insert((x, y, z));
            } else if plate_powered(st) {
                if __prof {
                    tickprof::PLATE_OFF.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                pw.insert((x, y, z));
            }
        }
        if let Some(t) = __tloop {
            tickprof::add("plates/on_loop", t.elapsed());
        }
        let __toff = __prof.then(std::time::Instant::now);
        let off: Vec<(i32, i32, i32)> = pw
            .iter()
            .filter(|&&(x, y, z)| !spots.contains(&(x, y, z)))
            .copied()
            .collect();
        for (x, y, z) in off {
            let st = overlay_block(x, y, z).unwrap_or(0);
            if plate_powered(st) {
                set_block(x, y, z, st + 1);
                note_block_update(x, y, z, st + 1);
                redstone_update(x, y - 1, z, false);
            }
            pw.remove(&(x, y, z));
        }
        if let Some(t) = __toff {
            tickprof::add("plates/off_loop", t.elapsed());
        }
    }
    // potion effects tick for ALL players (creative included — speed etc.
    // still apply there; damage effects are client-irrelevant in creative)
    {
        let __tsnap = __prof.then(std::time::Instant::now);
        let cls: Vec<Arc<Client>> = client_snapshot();
        if let Some(t) = __tsnap {
            tickprof::add("env/client_snapshot", t.elapsed());
        }
        let __tloop = __prof.then(std::time::Instant::now);
        for c in cls.iter() {
            effects::tick_effects(c);
        }
        if let Some(t) = __tloop {
            tickprof::add("env/effects_loop", t.elapsed());
        }
    }
    {
        if let Some(t) = __t {
            tickprof::add("env/plates", t.elapsed());
        }
        __t = __prof.then(std::time::Instant::now);
    }
    // Crop writes acquire SAVE_LOCK; grow_crops releases CROPS before any block write.
    grow_crops(t, |x, z| region.is_warm(x, z));
    let autosave = server_ticks(AUTOSAVE_GT);
    if AUTOSAVE_ENABLED.load(Ordering::Relaxed) && t % autosave == autosave - 1 {
        // The live server hands the snapshot to the save worker; everywhere
        // else this is the same synchronous save_world it always was.
        autosave_world();
        gc_backoff();
    }
    {
        if let Some(t) = __t {
            tickprof::add("env/crops", t.elapsed());
        }
        __t = __prof.then(std::time::Instant::now);
    }
    if GR_MOB_SPAWNING.load(Ordering::Relaxed) == 1 {
        let cycle = server_ticks(SPAWN_CYCLE_GT);
        // The smoother's mob budget gates each natural-spawn pass; at full
        // rate the gate is one float compare on every pass.
        let mob_scale = current_load_budget().mob_spawn_scale;
        if DIFFICULTY.load(Ordering::Relaxed) != 0
            && t % cycle == cycle / 2
            && (13000..=23000).contains(&wt)
            && mob_spawn_permitted(0, mob_scale)
        {
            spawn_nightly_hostile(t);
        }
        if t % nether_spawn_cycle() == 0 && mob_spawn_permitted(1, mob_scale) {
            spawn_nether_natural(t);
        }
        if !(13000..=23000).contains(&wt)
            && t % cycle == cycle / 4
            && mob_spawn_permitted(2, mob_scale)
        {
            spawn_passive_natural(t);
        }
        if t % cycle == cycle / 2 + 1 && mob_spawn_permitted(3, mob_scale) {
            spawn_aquatic_natural(t);
        }
        if t % cycle == cycle / 4 + 1 && mob_spawn_permitted(4, mob_scale) {
            spawn_end_natural(t);
        }
        if t % cycle == cycle - 1 && mob_spawn_permitted(5, mob_scale) {
            spawn_chunkgen_creatures(t);
        }
    }
    {
        // fishing bobbers: fly, land on water, bite after 3-7 s
        let mut fb = bobbers().lock().unwrap();
        for b in fb.iter_mut() {
            if !region.is_hot(b.x as i32, b.z as i32) {
                continue;
            }
            b.age += 1;
            let wst = name_to_state("water").unwrap_or(u32::MAX);
            let in_water =
                block_at(b.x.floor() as i32, b.y.floor() as i32, b.z.floor() as i32) == wst;
            if !in_water && b.dy.abs() + b.dx.abs() + b.dz.abs() > 0.01 {
                // Drag belongs on the velocity, not on the displacement, or the
                // horizontal velocity never decays. Values are unchanged here.
                b.dx *= 0.99;
                b.dy *= 0.99;
                b.dz *= 0.99;
                b.dy -= 0.03;
                b.x += b.dx;
                b.y += b.dy;
                b.z += b.dz;
            } else {
                b.dx = 0.0;
                b.dy = 0.0;
                b.dz = 0.0;
                if !b.bit && b.age > server_ticks(BOBBER_BITE_GT) as i32 {
                    b.bit = ((b.age as u64).wrapping_mul(2654435761) % 11) < 6;
                }
            }
            if b.age % server_ticks(BOBBER_PACKET_GT) as i32 == 0 {
                let (ax, ay, az, eid) = (b.x, b.y, b.z, b.eid);
                broadcast_seen(eid, |_c| {
                    (0x70, entity_teleport(eid, ax, ay, az, 0.0, 0.0, true))
                });
            }
        }
    }
    {
        // thrown pearls: fly, on expiry teleport owner
        let mut pl = pearls().lock().unwrap();
        let mut done: Vec<(i32, f64, f64, f64, u32)> = vec![];
        // A pearl flies in its thrower's dimension: an End pearl has to collide
        // with End blocks — most importantly an `end_gateway` beam.
        let pearl_dims: HashMap<i32, DimId> = client_snapshot()
            .iter()
            .map(|c| (c.eid, *c.dim.lock().unwrap_or_else(|e| e.into_inner())))
            .collect();
        for p0 in pl.iter_mut() {
            if !region.is_hot(p0.x as i32, p0.z as i32) {
                continue;
            }
            p0.ttl -= 1;
            let pdim = pearl_dims
                .get(&p0.owner)
                .copied()
                .unwrap_or(DimId::Overworld);
            let hit_state = block_at_dim(
                pdim,
                p0.x.floor() as i32,
                p0.y.floor() as i32,
                p0.z.floor() as i32,
            );
            if hit_state != 0 || p0.ttl <= 0 {
                done.push((p0.owner, p0.x, p0.y, p0.z, hit_state));
            } else {
                // vanilla ThrowableProjectile: scale 0.99, add -0.03, integrate.
                p0.dx *= 0.99;
                p0.dy *= 0.99;
                p0.dz *= 0.99;
                p0.dy -= 0.03;
                p0.x += p0.dx;
                p0.y += p0.dy;
                p0.z += p0.dz;
            }
            let (ax, ay, az, eid) = (p0.x, p0.y, p0.z, p0.eid);
            broadcast_seen(eid, |_c| {
                (0x70, entity_teleport(eid, ax, ay, az, 0.0, 0.0, true))
            });
        }
        for (owner, tx, ty, tz, hit_st) in done {
            let dead_pearl = pl.iter().find(|p0| p0.owner == owner).map(|p0| p0.eid);
            pl.retain(|p0| p0.owner != owner);
            if let Some(peid) = dead_pearl {
                broadcast_seen_destroy(peid);
            }
            let cls: Vec<Arc<Client>> = client_snapshot();
            if let Some(c) = cls.iter().find(|c| c.eid == owner) {
                if is_end_gateway_state(hit_st) {
                    // A pearl that lands in a gateway beam warps its owner to
                    // the beam's exit: no pearl damage, no fall damage.
                    end_gateway_warp(
                        c,
                        (tx.floor() as i32, ty.floor() as i32, tz.floor() as i32),
                    );
                    continue;
                }
                // The endermite appears where the player was standing when the
                // pearl landed — vanilla rolls it before the teleport happens.
                let (pre_x, pre_y, pre_z) = {
                    let p = c.pos.lock().unwrap_or_else(|e| e.into_inner());
                    (p.0, p.1, p.2)
                };
                let pdim = *c.dim.lock().unwrap_or_else(|e| e.into_inner());
                let (_, _, _, yaw, pitch, _) = *c.pos.lock().unwrap();
                if let Ok(mut w) = c.sock.lock() {
                    teleport_in_dimension(c, &mut w, (tx, ty, tz, yaw, pitch));
                    let mut h = c.hp.lock().unwrap();
                    *h = (*h - dmg_after(c, 5.0, 0)).max(0.0);
                    if *h <= 0.0 {
                        drop(h);
                        totem_check(c);
                        h = c.hp.lock().unwrap();
                    }
                    let hh = *h;
                    drop(h);
                    let _ = send_packet(&mut w, 0x5d, health_pkt_c(hh, c));
                }
                if hit_st != 0 {
                    pearl_landed_endermite(pdim, pre_x, pre_y, pre_z, world_rng::next_u64());
                }
            }
        }
    }
    {
        // eyes of ender: fly toward their stronghold target, then shatter or
        // drop, and players or ridden boats inside an end_gateway beam warp.
        tick_eyes_of_ender();
        tick_end_gateway_entries();
    }
    {
        // snowballs: fly, hit mob -> hurt flash
        let mut sl2 = snowballs().lock().unwrap();
        let mut done: Vec<ThrowHit> = vec![];
        for p0 in sl2.iter_mut() {
            if !region.is_hot(p0.x as i32, p0.z as i32) {
                continue;
            }
            p0.ttl -= 1;
            let mut hit_mob: Option<i32> = None;
            {
                let ms = mobs().lock().unwrap();
                for m in ms.iter().filter(|mob| mob.dim == DimId::Overworld) {
                    let mp = m.pos.lock().unwrap();
                    if (mp.0 - p0.x).abs() < 0.6
                        && (mp.2 - p0.z).abs() < 0.6
                        && p0.y > mp.1 - 0.3
                        && p0.y < mp.1 + 2.0
                    {
                        hit_mob = Some(m.eid);
                        break;
                    }
                }
            }
            let blocked = block_at(
                p0.x.floor() as i32,
                p0.y.floor() as i32,
                p0.z.floor() as i32,
            ) != 0;
            if hit_mob.is_some() || blocked || p0.ttl <= 0 {
                done.push((p0.eid, hit_mob, p0.dx, p0.dz, p0.egg, p0.x, p0.y, p0.z));
            } else {
                // Snowballs and eggs are ThrowableProjectile, like the pearl, so
                // they scale the velocity by 0.99 and use the 0.03 gravity. This
                // path had no drag at all and the wrong gravity.
                p0.dx *= 0.99;
                p0.dy *= 0.99;
                p0.dz *= 0.99;
                p0.dy -= 0.03;
                p0.x += p0.dx;
                p0.y += p0.dy;
                p0.z += p0.dz;
                broadcast_seen(p0.eid, |_c| {
                    (
                        0x70,
                        entity_teleport(p0.eid, p0.x, p0.y, p0.z, 0.0, 0.0, true),
                    )
                });
            }
        }
        for (seid, hit, pdx, pdz, is_egg, ix, iy, iz) in done {
            sl2.retain(|p0| p0.eid != seid);
            broadcast_seen_destroy(seid);
            // vanilla: an egg that lands has a 1/8 chance to hatch a chicken
            if is_egg && world_rng::next_u64().is_multiple_of(8) {
                let m = Arc::new(Mob {
                    dim: DimId::Overworld,
                    eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                    uuid: [0u8; 16],
                    etype: 19,
                    pos: Mutex::new((ix, iy, iz, 0.0, 0.0)),
                    hp: Mutex::new(4.0),
                    last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
                    hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
                    last_hurt_tick: Mutex::new(None),
                    vel: Mutex::new([0.0; 3]),
                    fuse: Mutex::new(0),
                    fire_ticks: Mutex::new(0),
                    love: Mutex::new(0),
                    air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
                });
                mobs().lock().unwrap().push(m);
            }
            if let Some(meid) = hit {
                broadcast_seen(meid, |_c| (0x24, hurt_anim(meid)));
                // vanilla: snowball/egg deal 0 damage but knock the target
                // along the throw direction (~0.5 blocks + velocity)
                let kl = (pdx * pdx + pdz * pdz).sqrt().max(0.01);
                let (kx, kz) = (pdx / kl * 0.5, pdz / kl * 0.5);
                if let Some(m) = mobs()
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|m| m.eid == meid)
                    .cloned()
                {
                    let (nx, ny, nz, yaw, pitch) = {
                        let mut p = m.pos.lock().unwrap();
                        p.0 += kx;
                        p.2 += kz;
                        *p
                    };
                    broadcast_seen(meid, |_c| {
                        (
                            0x5a,
                            velocity_pkt(meid, (kx * 6400.0) as i16, 1600, (kz * 6400.0) as i16),
                        )
                    });
                    broadcast_seen(meid, |_c| {
                        (0x70, entity_teleport(meid, nx, ny, nz, yaw, pitch, true))
                    });
                }
            }
        }
    }
    {
        // despawn xp orbs after 300s — plus the same hard cap as drops:
        // a farm can out-spawn the despawn window.
        //
        // Split under `MC_SPINE_TICK_PROFILE=1`: this block is most of `env/rest`
        // once a fixture has orbs, and the merge walks 27 cells per orb, so "rest"
        // alone cannot say whether the despawn pass or the merge is the cost.
        let __prof = tickprof::enabled();
        let mut __t = __prof.then(std::time::Instant::now);
        let mut xl = xps().lock().unwrap();
        let life = server_ticks(XP_LIFE_GT) as i32;
        for x in xl.iter_mut() {
            if region.is_hot(x.x as i32, x.z as i32) {
                x.age += 1;
            }
        }
        let exp: Vec<i32> = xl
            .iter()
            .filter(|x| x.age >= life && region.is_hot(x.x as i32, x.z as i32))
            .map(|x| x.eid)
            .collect();
        for eid in &exp {
            xl.retain(|x| x.eid != *eid);
            broadcast_seen_destroy(*eid);
        }
        while xl.len() > XP_CAP {
            let x = xl.remove(0);
            broadcast_seen_destroy(x.eid);
        }
        if let Some(t) = __t {
            tickprof::add("env/xp_age", t.elapsed());
        }
        __t = __prof.then(std::time::Instant::now);
        // vanilla merges nearby orbs into a single higher-value orb
        let merged = merge_xp_orbs(&mut xl);
        if let Some(t) = __t {
            tickprof::add("env/xp_merge", t.elapsed());
        }
        __t = __prof.then(std::time::Instant::now);
        for eid in &merged {
            xl.retain(|x| x.eid != *eid);
            broadcast_seen_destroy(*eid);
        }
        if let Some(t) = __t {
            tickprof::add("env/xp_destroy", t.elapsed());
        }
    }
    // #minecraft:tick functions run once per game tick (TICK_MS = 50ms = 20/s, the
    // vanilla rate)
    let ticks: Vec<String> = packs()
        .lock()
        .unwrap()
        .iter()
        .flat_map(|p| p.tick.clone())
        .collect();
    for f in ticks {
        run_function(&f, None, &mut None);
    }
    // /schedule due functions
    let due: Vec<String> = {
        let mut sf = sched_fns().lock().unwrap();
        let d: Vec<String> = sf
            .iter()
            .filter(|(_, dt)| *dt <= t)
            .map(|(f, _)| f.clone())
            .collect();
        sf.retain(|(_, dt)| *dt > t);
        d
    };
    for f in due {
        run_function(&f, None, &mut None);
    }
    {
        if let Some(t) = __t {
            tickprof::add("env/rest", t.elapsed());
        }
    }
}

// ---------- creature / aquatic / End natural spawning ----------
// The categories vanilla `NaturalSpawner` covers that the nightly monster and
// Nether cycles never reached: daytime animals on grass, water mobs in water
// columns, endermen in the End, and each eligible chunk's one-time creature
// herd from `spawn_mobs_for_chunk_generation`. The drivers keep the shared
// contract of `spawn_nightly_hostile`: players anchor eligible chunks, the
// category cap bounds the pass, and a spawn is one `with_save_guard` insert.

/// The `MobCategory` a live mob counts under, or `None` for `Misc` entities —
/// vanilla never lets those into the spawn-cap counts.
fn live_mob_category(etype: i64) -> Option<MobCategory> {
    Some(match entity_category_1211(etype) {
        EntityMobCategory::Monster => MobCategory::Monster,
        EntityMobCategory::Creature => MobCategory::Creature,
        EntityMobCategory::Ambient => MobCategory::Ambient,
        EntityMobCategory::Axolotls => MobCategory::Axolotls,
        EntityMobCategory::UndergroundWaterCreature => {
            MobCategory::UndergroundWaterCreature
        }
        EntityMobCategory::WaterCreature => MobCategory::WaterCreature,
        EntityMobCategory::WaterAmbient => MobCategory::WaterAmbient,
        EntityMobCategory::Misc => return None,
    })
}

/// Live mobs in `dim` counting toward `category`'s cap; dead entries still in
/// the list do not count against it.
fn live_category_count(dim: DimId, category: MobCategory) -> usize {
    mobs()
        .lock()
        .unwrap()
        .iter()
        .filter(|mob| {
            mob.dim == dim
                && *mob.hp.lock().unwrap() > 0.0
                && live_mob_category(mob.etype) == Some(category)
        })
        .count()
}

/// One natural spawn under the shared guards: `doMobSpawning`, the live-mob
/// ceiling and, for monsters, the despawn tracker. Creatures are persistent
/// in vanilla, so only monsters take the despawn-tracking arm.
fn live_natural_spawn(kind: &str, dim: DimId, pos: (f64, f64, f64, f32, f32)) -> bool {
    if GR_MOB_SPAWNING.load(Ordering::Relaxed) != 1 {
        return false;
    }
    let Some(spec) = entity_spec_by_name(kind) else {
        return false;
    };
    with_save_guard(|| {
        let mut current = mobs().lock().unwrap();
        if current.len() >= MOB_CAP {
            return false;
        }
        let mob = new_live_mob(spec.id, dim, pos, spec.max_health, None);
        if spec.category == EntityMobCategory::Monster {
            push_natural_hostile(&mut current, mob);
        } else {
            current.push(mob);
        }
        true
    })
}

/// The two-block clearance `SpawnPlacements.checkSpawnRules` requires for a
/// standing mob: no full collision shape, no liquid and no
/// `#prevent_mob_spawning_inside` block at the feet or the head.
fn live_spawn_clear(reg: &BReg, dim: DimId, x: i32, y: i32, z: i32) -> bool {
    let clear = |state| {
        !reg.is_collision_shape_full_block(state)
            && !reg.is_liquid(state)
            && !reg.in_tag(state, "minecraft:prevent_mob_spawning_inside")
    };
    clear(block_at_dim(dim, x, y, z)) && clear(block_at_dim(dim, x, y + 1, z))
}

/// `LevelReader.getMaxLocalRawBrightness`: the larger of the sky and block
/// light at the cell, from the settled/windowed light passes.
fn live_light_at(clients: &[Arc<Client>], dim: DimId, x: i32, y: i32, z: i32) -> i32 {
    let (cx, cz) = (x >> 4, z >> 4);
    let sky = nibble_at_dim(&sky_light_sections_dim(dim, cx, cz), dim, x, y, z);
    let sources = natural_spawn_light_sources(clients, dim, cx, cz);
    let block = nibble_at_dim(
        &block_light_sections_dim(dim, cx, cz, &sources),
        dim,
        x,
        y,
        z,
    );
    sky.max(block)
}

/// Non-spectator player positions in `dim`, the anchors every pass shares.
fn live_spawn_anchors(clients: &[Arc<Client>], dim: DimId) -> Vec<[f64; 3]> {
    clients
        .iter()
        .filter_map(|client| {
            if client.is_spectator() || *client.dim.lock().unwrap() != dim {
                return None;
            }
            let (x, y, z, ..) = *client.pos.lock().unwrap();
            Some([x, y, z])
        })
        .collect()
}

/// The union of spawnable chunks the vanilla cap formula reads.
fn live_spawn_chunks(players: &[[f64; 3]]) -> HashSet<(i32, i32)> {
    let mut chunks = HashSet::new();
    for &[x, _, z] in players {
        extend_chunk_square(
            &mut chunks,
            ((x.floor() as i32).div_euclid(16), (z.floor() as i32).div_euclid(16)),
            NATURAL_SPAWN_CHUNK_RADIUS,
        );
    }
    chunks
}

/// The nearest-player distance gate vanilla applies to every category, plus a
/// world-border membership check like the nightly pass'.
fn live_spawn_position_ok(
    players: &[[f64; 3]],
    spawn_chunks: &HashSet<(i32, i32)>,
    x: i32,
    y: i32,
    z: i32,
) -> bool {
    if !spawn_chunks.contains(&(x >> 4, z >> 4)) {
        return false;
    }
    let nearest_squared = players
        .iter()
        .map(|&[px, py, pz]| {
            (px - (f64::from(x) + 0.5)).powi(2)
                + (py - f64::from(y)).powi(2)
                + (pz - (f64::from(z) + 0.5)).powi(2)
        })
        .fold(f64::INFINITY, f64::min);
    if nearest_squared <= SPAWN_MIN_PLAYER_DIST_SQ {
        return false;
    }
    let (min_x, min_z, max_x, max_z) = world_border().lock().unwrap().bounds(border_now_ms());
    (min_x..max_x).contains(&f64::from(x)) && (min_z..max_z).contains(&f64::from(z))
}

/// One animal attempt: `Animal.checkAnimalSpawnRules` plus the brightness
/// gate (`isBrightEnoughToSpawn`, light above 8) under the creature cap.
fn live_passive_spawn_attempt(x: i32, y: i32, z: i32, light: i32, cap: usize, kind: &str) -> bool {
    if live_category_count(DimId::Overworld, MobCategory::Creature) >= cap {
        return false;
    }
    let Some(reg) = breg() else {
        return false;
    };
    if reg.name_of(block_at(x, y - 1, z)) != "minecraft:grass_block"
        || !live_spawn_clear(reg, DimId::Overworld, x, y, z)
        || light <= 8
    {
        return false;
    }
    live_natural_spawn(
        kind,
        DimId::Overworld,
        (f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5, 0.0, 0.0),
    )
}

/// Daytime animal spawning: one weighted pick from the biome's `creature`
/// spawners (the farm animals the runtime can simulate) plus a small group on
/// jittered grass. Runs only outside the night window; the caller gates it.
fn spawn_passive_natural(tick: u64) {
    let clients = client_snapshot();
    let players = live_spawn_anchors(&clients, DimId::Overworld);
    if players.is_empty() || breg().is_none() {
        return;
    }
    let spawn_chunks = live_spawn_chunks(&players);
    let cap = MobCategory::Creature.global_cap(spawn_chunks.len() as u32) as usize;
    if cap == 0 || live_category_count(DimId::Overworld, MobCategory::Creature) >= cap {
        return;
    }
    let attempt = SPAWN_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let &[px, _, pz] = &players[attempt as usize % players.len()];
    let angle = tick as f64 * 2.399;
    let bx = (px + angle.cos() * 32.0).floor() as i32;
    let bz = (pz + angle.sin() * 32.0).floor() as i32;
    if !live_spawn_position_ok(&players, &spawn_chunks, bx, 64, bz) {
        return;
    }
    let biome = worldgen::with_gen_dim(DimId::Overworld, |g| g.biome_name_at(bx, 64, bz))
        .flatten()
        .unwrap_or_else(|| "minecraft:plains".to_string());
    const FARM_ANIMALS: [&str; 4] = [
        "minecraft:sheep",
        "minecraft:cow",
        "minecraft:pig",
        "minecraft:chicken",
    ];
    let entries: Vec<&feature::SpawnerData> = feature::spawners_for(&biome, "creature")
        .unwrap_or_default()
        .iter()
        .filter(|entry| FARM_ANIMALS.contains(&entry.kind.as_str()))
        .collect();
    let total: i32 = entries.iter().map(|entry| entry.weight).sum();
    if total <= 0 {
        return;
    }
    let mut rng = generation_math::Lcg::new(
        WORLD_SEED
            .load(Ordering::Relaxed)
            .wrapping_add(tick.wrapping_mul(0x9E37_79B9_7F4A_7C15)),
    );
    let mut roll = rng.next_bound(total);
    let Some(entry) = entries.iter().find(|entry| {
        roll -= entry.weight;
        roll < 0
    }) else {
        return;
    };
    let span = (entry.max_count - entry.min_count + 1).max(1);
    let count = entry.min_count + rng.next_bound(span);
    for _ in 0..count {
        let x = bx + rng.next_bound(5) - 2;
        let z = bz + rng.next_bound(5) - 2;
        if !live_spawn_position_ok(&players, &spawn_chunks, x, 64, z) {
            continue;
        }
        let y = surface_y(x, z).floor() as i32;
        let light = live_light_at(&clients, DimId::Overworld, x, y, z);
        let kind = entry.kind.strip_prefix("minecraft:").unwrap_or(&entry.kind);
        live_passive_spawn_attempt(x, y, z, light, cap, kind);
    }
}

/// One water-mob attempt: the cell holds water in the sea-level band vanilla
/// water spawning reads (50..=63), under the category's cap.
fn live_aquatic_spawn_attempt(
    x: i32,
    y: i32,
    z: i32,
    category: MobCategory,
    cap: usize,
    kind: &str,
) -> bool {
    if !(50..=63).contains(&y)
        || live_category_count(DimId::Overworld, category) >= cap
    {
        return false;
    }
    let Some(reg) = breg() else {
        return false;
    };
    if reg.name_of(block_at(x, y, z)) != "minecraft:water" {
        return false;
    }
    live_natural_spawn(
        kind,
        DimId::Overworld,
        (f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5, 0.0, 0.0),
    )
}

/// Water-column spawning: squid for `water_creature` and a cod/salmon school
/// for `water_ambient`, each under its own cap like the vanilla category loop.
fn spawn_aquatic_natural(tick: u64) {
    let clients = client_snapshot();
    let players = live_spawn_anchors(&clients, DimId::Overworld);
    let Some(reg) = breg().filter(|_| !players.is_empty()) else {
        return;
    };
    let spawn_chunks = live_spawn_chunks(&players);
    for (category, kinds) in [
        (MobCategory::WaterCreature, &["squid"][..]),
        (MobCategory::WaterAmbient, &["cod", "salmon"][..]),
    ] {
        let cap = category.global_cap(spawn_chunks.len() as u32) as usize;
        if cap == 0 || live_category_count(DimId::Overworld, category) >= cap {
            continue;
        }
        let attempt = SPAWN_ATTEMPT.fetch_add(1, Ordering::Relaxed);
        let &[px, _, pz] = &players[attempt as usize % players.len()];
        let angle = tick as f64 * 2.399 + category.index() as f64;
        let bx = (px + angle.cos() * 32.0).floor() as i32;
        let bz = (pz + angle.sin() * 32.0).floor() as i32;
        if !live_spawn_position_ok(&players, &spawn_chunks, bx, 56, bz) {
            continue;
        }
        let mut rng = generation_math::Lcg::new(
            WORLD_SEED
                .load(Ordering::Relaxed)
                .wrapping_add(tick.wrapping_mul(0x85EB_CA6B)),
        );
        for _ in 0..4 {
            let x = bx + rng.next_bound(7) - 3;
            let z = bz + rng.next_bound(7) - 3;
            if !live_spawn_position_ok(&players, &spawn_chunks, x, 56, z) {
                continue;
            }
            let Some(y) = (50..=63)
                .rev()
                .find(|&y| reg.name_of(block_at(x, y, z)) == "minecraft:water")
            else {
                continue;
            };
            let kind = kinds[rng.next_bound(kinds.len() as i32) as usize];
            live_aquatic_spawn_attempt(x, y, z, category, cap, kind);
        }
    }
}

/// One enderman attempt in the End: `end_stone` below, clear standing room,
/// light level 7 or darker, under the hostile cap and a non-peaceful
/// difficulty — `Monster.checkMonsterSpawnRules` for `the_end`.
fn live_end_spawn_attempt(x: i32, y: i32, z: i32, light: i32, cap: usize) -> bool {
    if DIFFICULTY.load(Ordering::Relaxed) == 0
        || live_category_count(DimId::End, MobCategory::Monster) >= cap
    {
        return false;
    }
    let Some(reg) = breg() else {
        return false;
    };
    if reg.name_of(block_at_dim(DimId::End, x, y - 1, z)) != "minecraft:end_stone"
        || !live_spawn_clear(reg, DimId::End, x, y, z)
        || light > 7
    {
        return false;
    }
    live_natural_spawn(
        "enderman",
        DimId::End,
        (f64::from(x) + 0.5, f64::from(y), f64::from(z) + 0.5, 0.0, 0.0),
    )
}

/// The End's only natural spawn: endermen near End-resident players, gated by
/// `doMobSpawning`, difficulty and the hostile cap like every monster pass.
fn spawn_end_natural(tick: u64) {
    if DIFFICULTY.load(Ordering::Relaxed) == 0 {
        return;
    }
    let clients = client_snapshot();
    let players = live_spawn_anchors(&clients, DimId::End);
    if players.is_empty() {
        return;
    }
    let spawn_chunks = live_spawn_chunks(&players);
    let cap = MobCategory::Monster.global_cap(spawn_chunks.len() as u32) as usize;
    if cap == 0 || live_category_count(DimId::End, MobCategory::Monster) >= cap {
        return;
    }
    let attempt = SPAWN_ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let &[px, _, pz] = &players[attempt as usize % players.len()];
    let angle = tick as f64 * 2.399;
    let bx = (px + angle.cos() * 32.0).floor() as i32;
    let bz = (pz + angle.sin() * 32.0).floor() as i32;
    if !live_spawn_position_ok(&players, &spawn_chunks, bx, 64, bz) {
        return;
    }
    let Some(reg) = breg() else {
        return;
    };
    let mut rng = generation_math::Lcg::new(
        WORLD_SEED
            .load(Ordering::Relaxed)
            .wrapping_add(tick.wrapping_mul(0x27D4_EB2F)),
    );
    for _ in 0..4 {
        let x = bx + rng.next_bound(7) - 3;
        let z = bz + rng.next_bound(7) - 3;
        // The End surface: the first non-air cell below the spawn band.
        let Some(y) = (DimId::End.min_y()..=128)
            .rev()
            .find(|&y| !reg.is_air(block_at_dim(DimId::End, x, y, z)))
            .map(|y| y + 1)
        else {
            continue;
        };
        if !live_spawn_position_ok(&players, &spawn_chunks, x, y, z) {
            continue;
        }
        let light = live_light_at(&clients, DimId::End, x, y, z);
        live_end_spawn_attempt(x, y, z, light, cap);
    }
}

/// Chunks whose first-visit creature herd already ran, per dimension. Vanilla
/// attaches the pass to chunk creation; the live server marks each chunk the
/// pass has seen, so a re-entered chunk never spawns a second herd.
fn spawned_creature_chunks() -> &'static Mutex<HashSet<DimChunkKey>> {
    static DONE: OnceLock<Mutex<HashSet<DimChunkKey>>> = OnceLock::new();
    DONE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// The live `ChunkGenerationWorld` adapter for
/// `spawn_mobs_for_chunk_generation`: surface placement against generated or
/// overlaid terrain, and each accepted creature joins the live mob list.
struct LiveChunkGenWorld {
    dim: DimId,
    added: usize,
}

impl spawner::ChunkGenerationWorld for LiveChunkGenWorld {
    fn spawn_y(&mut self, x: i32, z: i32) -> i32 {
        let base = if self.dim == DimId::Overworld {
            surface_y(x, z).floor() as i32
        } else {
            height_at_dim(self.dim, x, z) + 1
        };
        // An edited block above the generated surface is the standing point,
        // like the world-surface heightmap vanilla herds read.
        let world = timed_world();
        world
            .get(&(self.dim, x >> 4, z >> 4))
            .and_then(|cells| {
                let reg = breg();
                (base..self.dim.min_y() + self.dim.height())
                    .rev()
                    .find(|&y| {
                        cells
                            .get(&local_idx(x, y, z))
                            .is_some_and(|&st| reg.is_none_or(|reg| !reg.is_air(st)))
                    })
                    .map(|y| y + 1)
            })
            .unwrap_or(base)
    }

    fn ground_spawnable(&mut self, _kind: &str, pos: (i32, i32, i32)) -> bool {
        let Some(reg) = breg() else {
            return false;
        };
        let below = block_at_dim(self.dim, pos.0, pos.1 - 1, pos.2);
        (reg.name_of(below) == "minecraft:grass_block" || reg.side_solid(below, 1))
            && live_spawn_clear(reg, self.dim, pos.0, pos.1, pos.2)
    }

    fn water_spawnable(&mut self, _kind: &str, pos: (i32, i32, i32)) -> bool {
        breg().is_some_and(|reg| {
            reg.name_of(block_at_dim(self.dim, pos.0, pos.1, pos.2)) == "minecraft:water"
        })
    }

    fn air_spawnable(&mut self, _kind: &str, pos: (i32, i32, i32)) -> bool {
        let Some(reg) = breg() else {
            return false;
        };
        reg.is_air(block_at_dim(self.dim, pos.0, pos.1, pos.2))
            && reg.is_air(block_at_dim(self.dim, pos.0, pos.1 + 1, pos.2))
    }

    fn add_creature(&mut self, kind: &str, pos: (i32, i32, i32)) {
        if live_natural_spawn(
            kind.strip_prefix("minecraft:").unwrap_or(kind),
            self.dim,
            (f64::from(pos.0) + 0.5, f64::from(pos.1), f64::from(pos.2) + 0.5, 0.0, 0.0),
        ) {
            self.added += 1;
        }
    }
}

/// `NaturalSpawner.spawnMobsForChunkGeneration` for one generated chunk: the
/// call runs at most once per dimension+chunk, recorded in
/// `spawned_creature_chunks` before the gamerule gate — a chunk created while
/// `doMobSpawning` was off does not retroactively spawn a herd, matching the
/// vanilla chunk-creation attachment. Returns whether the pass ran.
fn live_chunkgen_visit(dim: DimId, cx: i32, cz: i32) -> bool {
    if !spawned_creature_chunks().lock().unwrap().insert((dim, cx, cz)) {
        return false;
    }
    live_chunkgen_run(dim, cx, cz, None)
}

/// Run the chunk-generation creature pass for the biome at the chunk centre.
/// Split from the once-per-chunk gate so a test can pin the spawn mechanics
/// separately from the visit tracking. Returns whether the pass executed.
fn live_chunkgen_run(dim: DimId, cx: i32, cz: i32, probability: Option<f32>) -> bool {
    if GR_MOB_SPAWNING.load(Ordering::Relaxed) != 1 {
        return false;
    }
    let biome = worldgen::with_gen_dim(dim, |g| g.biome_name_at(cx * 16 + 8, 64, cz * 16 + 8))
        .flatten()
        .unwrap_or_else(|| "minecraft:plains".to_string());
    let mut world = LiveChunkGenWorld { dim, added: 0 };
    // Per-chunk seeded like the decoration random vanilla draws from: the same
    // chunk replays the same herd under the same seed.
    let mut rng = generation_math::Lcg::new(
        WORLD_SEED
            .load(Ordering::Relaxed)
            .wrapping_add((cx as u64 & 0xffff_ffff) << 32 ^ (cz as u64 & 0xffff_ffff)),
    );
    spawner::spawn_mobs_for_chunk_generation(
        cx,
        cz,
        &biome,
        probability,
        |b, category| {
            feature::spawners_for(b, category)
                .unwrap_or_default()
                .iter()
                .map(|entry| spawner::CreatureSpawnerData {
                    kind: entry.kind.clone(),
                    min_count: entry.min_count,
                    max_count: entry.max_count,
                    weight: entry.weight,
                })
                .collect()
        },
        &mut world,
        &mut rng,
    );
    true
}

/// First-visit creature herds for the Overworld chunks players can see. Each
/// chunk runs `spawn_mobs_for_chunk_generation` once; `doMobSpawning` gates
/// the pass inside `live_chunkgen_run`.
fn spawn_chunkgen_creatures(_tick: u64) {
    let mut chunks = HashSet::new();
    for client in client_snapshot() {
        if client.is_spectator() || *client.dim.lock().unwrap() != DimId::Overworld {
            continue;
        }
        let (x, _, z, ..) = *client.pos.lock().unwrap();
        extend_chunk_square(
            &mut chunks,
            ((x.floor() as i32).div_euclid(16), (z.floor() as i32).div_euclid(16)),
            4,
        );
    }
    for (cx, cz) in chunks {
        live_chunkgen_visit(DimId::Overworld, cx, cz);
    }
}

// ~1/30 chance per real second, with the per-tick denominator scaled to TPS.
fn grow_crops(t: u64, is_warm: impl Fn(i32, i32) -> bool) {
    let transitions = {
        let mut cg = crops().lock().unwrap();
        let keys: Vec<DimBlockPos> = cg
            .keys()
            .copied()
            .filter(|&(_, x, _, z)| is_warm(x, z))
            .collect();
        let mut transitions = Vec::new();
        for pos @ (dim, x, y, z) in keys {
            let age = cg.get(&pos).copied().unwrap_or(0);
            let hash = ((x as i64).wrapping_mul(31)
                ^ (z as i64).wrapping_mul(37)
                ^ (y as i64)
                ^ t as i64) as u64;
            if hash.is_multiple_of(CROP_GROWTH_DENOMINATOR) && age < 7 {
                cg.insert(pos, age + 1);
                transitions.push((dim, x, y, z, 4278 + age as u32 + 1));
            }
        }
        transitions
    };
    for (dim, x, y, z, state) in transitions {
        set_block_dim(dim, x, y, z, state);
        note_block_update_dim(dim, x, y, z, state);
    }
}


// minimal redstone: lever powers 6 adjacent cells -> iron door opens/closes, piston pushes one block
fn redstone_update(x: i32, y: i32, z: i32, powered: bool) {
    for (dx, dy, dz) in [
        (1i32, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ] {
        let (bx, by, bz) = (x + dx, y + dy, z + dz);
        let st = block_at(bx, by, bz);
        if (5652..=5715).contains(&st) {
            // iron door: open while powered (both halves)
            for oy in [-1i32, 0, 1] {
                let hst = block_at(bx, by + oy, bz);
                if !(5652..=5715).contains(&hst) {
                    continue;
                }
                let open_now = (hst - 5652) & 2 == 0;
                if open_now != powered {
                    let nst = if powered { hst & !2 } else { hst | 2 };
                    set_block(bx, by + oy, bz, nst);
                    note_block_update(bx, by + oy, bz, nst);
                }
            }
            continue;
        }
        if (2011..=2022).contains(&st) {
            // piston
            let off = st - 2011; // 0-5=extended, 6-11=retracted
            let facing = (off % 6) as i32; // 0 n,1 e,2 s,3 w,4 up,5 down
            let extended = off < 6;
            let (fx, fy, fz) = match facing {
                0 => (0, 0, -1),
                1 => (1, 0, 0),
                2 => (0, 0, 1),
                3 => (-1, 0, 0),
                4 => (0, 1, 0),
                _ => (0, -1, 0),
            };
            let (px, py, pz) = (bx + fx, by + fy, bz + fz);
            if powered && !extended {
                let nst = st - 6;
                set_block(bx, by, bz, nst);
                note_block_update(bx, by, bz, nst);
                let front = block_at(px, py, pz);
                let (qx, qy, qz) = (bx + fx * 2, by + fy * 2, bz + fz * 2);
                let dest = block_at(qx, qy, qz);
                let head = 2023 + facing as u32 * 4; // piston_head, normal type
                if front != 0 && dest == 0 {
                    place_block(qx, qy, qz, front);
                    place_block(px, py, pz, head);
                } else if front == 0 {
                    set_block(px, py, pz, head);
                    note_block_update(px, py, pz, head);
                }
            } else if !powered && extended {
                let nst = st + 6;
                set_block(bx, by, bz, nst);
                note_block_update(bx, by, bz, nst);
                if (2023..=2046).contains(&block_at(px, py, pz)) {
                    set_block(px, py, pz, 0);
                    note_block_update(px, py, pz, 0);
                }
            }
            continue;
        }
    }
}
// ---------- fake players (carpet-actor equivalent for parity fixtures) ----------
struct Actor {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    eid: i32,
    pos: Mutex<(f64, f64, f64)>,
    look: Mutex<Option<(i32, i32, i32)>>,
    action: Mutex<&'static str>, // "" | "attack" | "use"
    held: Mutex<Stack>,
    inv: Mutex<HashMap<i16, Stack>>,
}
static ACTORS: OnceLock<Mutex<HashMap<String, Arc<Actor>>>> = OnceLock::new();
fn actors() -> &'static Mutex<HashMap<String, Arc<Actor>>> {
    ACTORS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn actor_tick() {
    let acts: Vec<Arc<Actor>> = actors().lock().unwrap().values().cloned().collect();
    for a in acts {
        let act = *a.action.lock().unwrap();
        let look = *a.look.lock().unwrap();
        let (ax, ay, az) = *a.pos.lock().unwrap();
        if let Some((tx, ty, tz)) = look {
            match act {
                "attack" => {
                    let st = block_at(tx, ty, tz);
                    if st != 0 {
                        for it in if GR_DO_TILE_DROPS.load(Ordering::Relaxed) != 0 {
                            block_drops(st)
                        } else {
                            Vec::new()
                        } {
                            let d = Drop {
                                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                x: tx as f64 + 0.5,
                                y: ty as f64 + 0.4,
                                z: tz as f64 + 0.5,
                                item: it,
                                cnt: 1,
                                comps: Vec::new(),
                                age: 0,
                                target: None,
                                thrower: None,
                            };
                            drops().lock().unwrap().push(d);
                            // spawn streams via interest_tick
                        }
                        set_block(tx, ty, tz, 0);
                        note_block_update(tx, ty, tz, 0);
                    }
                }
                "use" => {
                    // place held block on the target face toward the actor (dominant horizontal axis)
                    let (hx, hz) = (ax - tx as f64, az - tz as f64);
                    let (ox, oz) = if hx.abs() >= hz.abs() {
                        (hx.signum() as i32, 0)
                    } else {
                        (0, hz.signum() as i32)
                    };
                    let (px, py, pz) = (tx + ox, ty, tz + oz);
                    let (item, cnt, _hc) = a.held.lock().unwrap().clone();
                    if item != 0 && cnt > 0 && block_at(px, py, pz) == 0 {
                        if let Some(st) = item_to_state(item) {
                            set_block(px, py, pz, st);
                            note_block_update(px, py, pz, st);
                            let mut h = a.held.lock().unwrap();
                            h.1 = h.1.saturating_sub(1);
                            if h.1 == 0 {
                                h.0 = 0;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        // absorb nearby drops via shared 1.21.1 AABB + target pickup gate
        {
            let actor_aabb = player_touch_aabb(ax, ay, az, false, false);
            let mut inv = a.inv.lock().unwrap();
            let mut ds = drops().lock().unwrap();
            let mut gone: Vec<i32> = vec![];
            let mut shrunk: Vec<(i32, Vec<u8>)> = vec![];
            for d in ds.iter_mut() {
                // vanilla: freshly spawned/thrown drops can't be picked up for ~10 ticks
                if !d.pickup_delay_elapsed() {
                    continue;
                }
                if d.can_be_picked_up_by(None, false, true, actor_aabb) {
                    // vanilla takes the whole stack; a mergeable slot is
                    // filled to 64 and the entity keeps the remainder
                    let smax = item_stack_max(d.item, &d.comps) as u8;
                    let slot = inv
                        .iter()
                        .find(|(_, v)| v.0 == d.item && v.1 < smax && v.2 == d.comps)
                        .map(|(k, _)| *k)
                        .or_else(|| (0..36i16).find(|sl| !inv.contains_key(sl)));
                    let Some(slot) = slot else { continue }; // full inv -> item stays
                    let cur = inv.get(&slot).map(|v| v.1).unwrap_or(0);
                    let room = (smax - cur).min(d.cnt);
                    inv.insert(slot, (d.item, cur + room, d.comps.clone()));
                    d.cnt -= room;
                    if d.cnt == 0 {
                        gone.push(d.eid);
                    } else {
                        shrunk.push((d.eid, drop_meta_pkt(d)));
                    }
                }
            }
            drop(inv);
            for e in gone {
                ds.retain(|d| d.eid != e);
                broadcast_seen_destroy(e);
            }
            for (eid, pkt) in shrunk {
                broadcast_seen(eid, move |_c| (0x58, pkt.clone()));
            }
        }
    }
}
// blocks that flowing water washes away into drops (vanilla replaceables):
// grass family + seagrass, flowers + mushrooms, torches, crops, snow
// layers, sugar cane
fn water_washable(st: u32) -> bool {
    matches!(st, 2005..=2016 | 2075..=2090 | 2355..=2359 | 4278..=4285 | 5772..=5779 | 5799..=5814)
}
fn water_intoable(st: u32) -> bool {
    st == 0 || water_washable(st)
}

fn wash_away(x: i32, y: i32, z: i32) {
    let st = block_at(x, y, z);
    if !water_washable(st) {
        return;
    }
    for item in block_drops(st) {
        let d = Drop {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            x: x as f64 + 0.5,
            y: y as f64 + 0.4,
            z: z as f64 + 0.5,
            item,
            cnt: 1,
            comps: Vec::new(),
            age: 0,
            target: None,
            thrower: None,
        };
        drops().lock().unwrap().push(d);
    }
}
// vanilla fluid contact: lava into water solidifies — source-level lava
// meeting water makes obsidian, flowing lava makes cobblestone, falling
// lava onto water makes stone. Water into lava source makes obsidian,
// into flowing lava makes cobblestone.
fn solidify_on_contact(lx: i32, ly: i32, lz: i32, lava_lvl: u32) -> bool {
    let mut hit = false;
    for (dx, dy, dz) in [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ] {
        if (80..=95).contains(&block_at(lx + dx, ly + dy, lz + dz)) {
            hit = true;
            break;
        }
    }
    if !hit {
        return false;
    }
    let st = if lava_lvl == 0 { 2354 } else { 14 }; // obsidian / cobblestone
    set_block(lx, ly, lz, st);
    note_block_update(lx, ly, lz, st);
    true
}

fn lava_tick() {
    // vanilla: lava flows like water but ~6x slower and only 3 blocks
    // horizontally in the overworld. One server tick is one game tick, so gating
    // to every 30th tick (server_ticks(FLUID_TICK_GT)) is vanilla's 1.5s lava
    // re-flow cadence.
    // Only the cells that come due are visited; a backed-off cell costs nothing.
    // Every exit re-queues except removal from the registry.
    let due = {
        let mut q = lava_queue().lock().unwrap();
        q.due()
    };
    if due.is_empty() {
        return;
    }
    let region = region_guard();
    for (x, y, z) in due {
        if !region.is_warm(x, z) || !sched_ready(x, y, z) {
            requeue_lava(x, y, z);
            continue;
        }
        let st = block_at(x, y, z);
        if !(96..=111).contains(&st) {
            lavas().lock().unwrap().remove(&(x, y, z));
            continue;
        }
        let lvl = st - 96;
        if water_intoable(block_at(x, y - 1, z)) {
            if !solidify_on_contact(x, y - 1, z, 8) {
                wash_away(x, y - 1, z);
                set_block(x, y - 1, z, 104);
                note_block_update(x, y - 1, z, 104);
            }
            sched_result(x, y, z, true);
            requeue_lava(x, y, z);
            continue;
        }
        if lvl >= 3 || (8..=15).contains(&lvl) {
            sched_result(x, y, z, false);
            requeue_lava(x, y, z);
            continue;
        }
        let mut spread = false;
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            if water_intoable(block_at(x + dx, y, z + dz)) && block_at(x + dx, y - 1, z + dz) != 0 {
                if !solidify_on_contact(x + dx, y, z + dz, lvl + 1) {
                    wash_away(x + dx, y, z + dz);
                    let nst = 96 + lvl + 1;
                    set_block(x + dx, y, z + dz, nst);
                    note_block_update(x + dx, y, z + dz, nst);
                }
                spread = true;
            }
        }
        sched_result(x, y, z, spread);
        requeue_lava(x, y, z);
    }
}
fn water_tick() {
    // vanilla-ish: source(80) spreads horizontally into air, level+1, max level 7
    // Only the cells that come due are visited, so a tick costs the cells that
    // need work rather than every cell that exists. Every exit below re-queues the
    // cell for its next visit, except removal from the registry, which drops it.
    let due = {
        let mut q = water_queue().lock().unwrap();
        q.due()
    };
    if due.is_empty() {
        return;
    }
    let region = region_guard();
    for (x, y, z) in due {
        if !region.is_warm(x, z) || !sched_ready(x, y, z) {
            requeue_water(x, y, z);
            continue;
        }
        let st = block_at(x, y, z);
        if !(80..=95).contains(&st) {
            waters().lock().unwrap().remove(&(x, y, z));
            continue;
        }
        let lvl = st - 80;
        let below = block_at(x, y - 1, z);
        if (96..=111).contains(&below) {
            let st = if below == 96 { 2354u32 } else { 14 };
            set_block(x, y - 1, z, st);
            note_block_update(x, y - 1, z, st);
            sched_result(x, y, z, true);
            requeue_water(x, y, z);
            continue;
        }
        if water_intoable(below) {
            // falls downward as level-8 falling water
            wash_away(x, y - 1, z);
            set_block(x, y - 1, z, 88);
            note_block_update(x, y - 1, z, 88);
            sched_result(x, y, z, true);
            requeue_water(x, y, z);
            continue;
        }
        if lvl >= 7 {
            sched_result(x, y, z, false);
            requeue_water(x, y, z);
            continue;
        }
        let mut spread = false;
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let cst = block_at(x + dx, y, z + dz);
            if (96..=111).contains(&cst) {
                // water flowing into lava: source -> obsidian, flowing -> cobble
                let st = if cst == 96 { 2354 } else { 14 };
                set_block(x + dx, y, z + dz, st);
                note_block_update(x + dx, y, z + dz, st);
                spread = true;
                continue;
            }
            if water_intoable(cst) && block_at(x + dx, y - 1, z + dz) != 0 {
                wash_away(x + dx, y, z + dz);
                let nst = 80 + lvl + 1;
                set_block(x + dx, y, z + dz, nst);
                note_block_update(x + dx, y, z + dz, nst);
                spread = true;
            }
        }
        sched_result(x, y, z, spread);
        requeue_water(x, y, z);
    }
}
thread_local! {
    // Goal trait objects stay on the server tick thread. Network handlers own
    // interaction and death events, so the separate behavior store is shared.
    static LIVE_MOB_AI: std::cell::RefCell<HashMap<i32, MobAiProfile>> = Default::default();
}

struct LiveMobState {
    behavior: Option<MobBehaviorState>,
    blaze_charged: bool,
    potions: Vec<(WitchPotion, u64)>,
}

impl LiveMobState {
    fn new(etype: i64) -> Self {
        Self {
            behavior: MobBehaviorState::for_etype(etype),
            blaze_charged: false,
            potions: Vec::new(),
        }
    }
}

fn live_mob_states() -> &'static Mutex<HashMap<i32, LiveMobState>> {
    static STATES: OnceLock<Mutex<HashMap<i32, LiveMobState>>> = OnceLock::new();
    STATES.get_or_init(Mutex::default)
}

fn insert_live_mob(
    etype: i64,
    dim: DimId,
    pos: (f64, f64, f64, f32, f32),
    hp: f32,
    slime_size: Option<SlimeSize>,
) -> bool {
    let mut mobs = mobs().lock().unwrap();
    if mobs.len() >= MOB_CAP {
        return false;
    }
    mobs.push(new_live_mob(etype, dim, pos, hp, slime_size));
    true
}

// Caller holds the mob list lock so an entity and its state become visible together.
fn new_live_mob(
    etype: i64,
    dim: DimId,
    pos: (f64, f64, f64, f32, f32),
    hp: f32,
    slime_size: Option<SlimeSize>,
) -> Arc<Mob> {
    let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
    let mut state = LiveMobState::new(etype);
    if let (Some(MobBehaviorState::Slime { behavior, .. }), Some(size)) =
        (&mut state.behavior, slime_size)
    {
        behavior.size = size;
    }
    if state.behavior.is_some() {
        live_mob_states().lock().unwrap().insert(eid, state);
    }
    // Entity IDs are unique within the running world, including split children.
    let mut uuid = [0; 16];
    uuid[12..].copy_from_slice(&eid.to_be_bytes());
    let last_action = Instant::now() - Duration::from_secs(10);
    Arc::new(Mob {
        dim,
        eid,
        uuid,
        etype,
        pos: Mutex::new(pos),
        hp: Mutex::new(hp),
        last_hit: Mutex::new(last_action),
        hurt_at: Mutex::new(last_action),
        last_hurt_tick: Mutex::new(None),
        vel: Mutex::new([0.0; 3]),
        fuse: Mutex::new(0),
        fire_ticks: Mutex::new(0),
        love: Mutex::new(0),
        air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
    })
}

fn remove_live_mob(eid: i32) -> Option<(Arc<Mob>, Option<LiveMobState>)> {
    let mut mobs = mobs().lock().unwrap();
    let index = mobs.iter().position(|mob| mob.eid == eid)?;
    let state = live_mob_states().lock().unwrap().remove(&eid);
    let mob = mobs.remove(index);
    drop(mobs);
    mob_custom_names()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&eid);
    // Vanilla `Entity.remove` drops the leash at the dying entity; any tether
    // this mob anchored (a mob cannot hold a lead, but the generic table does
    // not know that) detaches with its own drop at the other end.
    let (x, y, z, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
    detach_tethers_involving(eid, Some((x, y, z)));
    Some((mob, state))
}

fn convert_live_mob(mob: &Mob, dim: DimId, etype: i64) {
    let pos = *mob.pos.lock().unwrap();
    let mut mobs = mobs().lock().unwrap();
    let Some(slot) = mobs.iter_mut().find(|entry| entry.eid == mob.eid) else {
        return;
    };
    if *mob.hp.lock().unwrap() <= 0.0 {
        return;
    }
    let hp = default_mob_hp(entity_spec_by_id(etype).unwrap().name);
    *slot = new_live_mob(etype, dim, pos, hp, None);
    live_mob_states().lock().unwrap().remove(&mob.eid);
    drop(mobs);
    broadcast_seen_destroy(mob.eid);
}

// ---------- entity tethers (vanilla lead / generic chain tether) ----------
/// What a tether is anchored to. `Entity` is a live entity holding the lead —
/// normally the player that leashed the mob; `FenceKnot` is the `leash_knot`
/// entity (`leash_knots()` owns its registry entry) sitting on a fence block.
#[derive(Clone, Copy, Debug, PartialEq)]
enum TetherAnchor {
    Entity(i32),
    FenceKnot {
        dim: DimId,
        pos: (i32, i32, i32),
        knot_eid: i32,
    },
}

/// One tethered entity: `target_eid` is the mob on the rope. The vanilla lead
/// is `max_length 10` / `elastic_start 6` / `break_on_stretch`; a generic chain
/// tether (e.g. a mod's dragon chain) sets `break_on_stretch = false` and a
/// longer `max_length`, which clamps the target onto the sphere instead of
/// snapping. `drop_item_on_break` is the item spawned when the tether detaches
/// (`lead` for the vanilla lead, `None` for a chain that must not drop).
#[derive(Clone, Copy)]
struct EntityTether {
    target_eid: i32,
    anchor: TetherAnchor,
    max_length: f64,
    elastic_start: f64,
    break_on_stretch: bool,
    drop_item_on_break: Option<i32>,
}
static ENTITY_TETHERS: OnceLock<Mutex<HashMap<i32, EntityTether>>> = OnceLock::new();
fn entity_tethers() -> &'static Mutex<HashMap<i32, EntityTether>> {
    ENTITY_TETHERS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Custom names applied by `name_tag`, keyed by the mob's entity id. Metadata
/// slot 2/3 broadcasts keep the viewers in sync; the entry dies with the mob.
static MOB_CUSTOM_NAMES: OnceLock<Mutex<HashMap<i32, String>>> = OnceLock::new();
fn mob_custom_names() -> &'static Mutex<HashMap<i32, String>> {
    MOB_CUSTOM_NAMES.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Live `leash_knot` entities: knot eid -> (dimension, fence block position).
static LEASH_KNOTS: OnceLock<Mutex<HashMap<i32, (DimId, BlockPos)>>> = OnceLock::new();
fn leash_knots() -> &'static Mutex<HashMap<i32, (DimId, BlockPos)>> {
    LEASH_KNOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Protocol 767 S2C `set_entity_link` (`0x59`): the attached entity id then the
/// holding entity id, both as big-endian `i32`. A holding id of `-1` detaches
/// (`proto/src/data_v767.rs` pins the packet id).
fn set_entity_link_pkt(attached_eid: i32, holding_eid: i32) -> Vec<u8> {
    let mut p = Vec::with_capacity(8);
    p.extend_from_slice(&attached_eid.to_be_bytes());
    p.extend_from_slice(&holding_eid.to_be_bytes());
    p
}

/// The vanilla lead shape of `EntityTether`: ten blocks, elastic from six,
/// snaps on stretch and drops a lead item where the tethered mob stood.
fn vanilla_lead_tether(target_eid: i32, anchor: TetherAnchor) -> EntityTether {
    EntityTether {
        target_eid,
        anchor,
        max_length: 10.0,
        elastic_start: 6.0,
        break_on_stretch: true,
        drop_item_on_break: name_to_item("lead").map(|id| id as i32),
    }
}

/// Position + dimension of any entity id this server tracks: players first,
/// then mobs. `None` means the entity is gone (disconnect/death).
fn entity_pos_dim(eid: i32) -> Option<(DimId, f64, f64, f64)> {
    let clients = clients().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = clients.iter().find(|c| c.eid == eid) {
        let p = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
        let dim = *c.dim.lock().unwrap_or_else(|e| e.into_inner());
        return Some((dim, p.0, p.1, p.2));
    }
    drop(clients);
    let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
    let m = mobs.iter().find(|m| m.eid == eid)?;
    let p = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
    Some((m.dim, p.0, p.1, p.2))
}

/// A mob's position + dimension by entity id — the tether target lookup.
fn mob_tether_pos(eid: i32) -> Option<(DimId, f64, f64, f64)> {
    let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
    let m = mobs.iter().find(|m| m.eid == eid)?;
    let p = *m.pos.lock().unwrap_or_else(|e| e.into_inner());
    Some((m.dim, p.0, p.1, p.2))
}

/// Where the anchor end of a tether is, or `None` when the holder is gone. A
/// knot only counts while its `leash_knots()` entry lives.
fn tether_anchor_pos(anchor: &TetherAnchor) -> Option<(DimId, f64, f64, f64)> {
    match *anchor {
        TetherAnchor::Entity(eid) => entity_pos_dim(eid),
        TetherAnchor::FenceKnot {
            dim,
            pos,
            knot_eid,
        } => leash_knots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&knot_eid)
            .then_some((
                dim,
                pos.0 as f64 + 0.5,
                pos.1 as f64 + 0.5,
                pos.2 as f64 + 0.5,
            )),
    }
}

/// Attach a tether and tell every tracker of the target the vanilla way:
/// `0x59 SetEntityLink(attached, holding)`.
fn attach_tether(tether: EntityTether) {
    let holding = match tether.anchor {
        TetherAnchor::Entity(eid) => eid,
        TetherAnchor::FenceKnot { knot_eid, .. } => knot_eid,
    };
    let target = tether.target_eid;
    entity_tethers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(target, tether);
    broadcast_seen(target, move |_c| {
        (0x59, set_entity_link_pkt(target, holding))
    });
}

/// Detach `target_eid`: broadcast `0x59(target, -1)` and spawn
/// `drop_item_on_break` at `drop_pos` when both are set (vanilla `dropLeash`
/// spawns the lead at the entity). Returns the removed tether.
fn detach_tether(target_eid: i32, drop_pos: Option<(f64, f64, f64)>) -> Option<EntityTether> {
    let tether = entity_tethers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&target_eid)?;
    broadcast_seen(target_eid, move |_c| {
        (0x59, set_entity_link_pkt(target_eid, -1))
    });
    if let (Some(item), Some((x, y, z))) = (tether.drop_item_on_break, drop_pos) {
        drops().lock().unwrap_or_else(|e| e.into_inner()).push(Drop {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            x,
            y,
            z,
            item: i64::from(item),
            cnt: 1,
            comps: Vec::new(),
            age: 0,
            target: None,
            thrower: None,
        });
    }
    Some(tether)
}

/// Detach every tether anchored to or targeting `eid` — the disconnect and
/// mob-death cleanup. Leads drop where the tethered mob stands;
/// `drop_pos_of_eid` supplies the position when `eid` itself is the target
/// (its own record is already gone when this runs).
fn detach_tethers_involving(eid: i32, drop_pos_of_eid: Option<(f64, f64, f64)>) {
    let work: Vec<(i32, bool)> = {
        let map = entity_tethers().lock().unwrap_or_else(|e| e.into_inner());
        map.values()
            .filter(|t| {
                t.target_eid == eid
                    || matches!(t.anchor, TetherAnchor::Entity(a) if a == eid)
            })
            .map(|t| (t.target_eid, t.target_eid == eid))
            .collect()
    };
    for (target, is_self) in work {
        let drop_at = if is_self {
            drop_pos_of_eid
        } else {
            mob_tether_pos(target).map(|(_, x, y, z)| (x, y, z))
        };
        detach_tether(target, drop_at);
    }
}

/// The knot entity sitting on `(dim, pos)`, if one is already spawned there.
fn knot_eid_at(dim: DimId, pos: BlockPos) -> Option<i32> {
    leash_knots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(_, &(d, p))| d == dim && p == pos)
        .map(|(&eid, _)| eid)
}

/// Vanilla `LeashFenceKnotEntity.getOrCreateKnot`: reuse the knot already on
/// the fence, or spawn one. Streaming to observers rides `interest_tick`.
fn ensure_leash_knot(dim: DimId, pos: BlockPos) -> i32 {
    if let Some(eid) = knot_eid_at(dim, pos) {
        return eid;
    }
    let knot_eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
    leash_knots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(knot_eid, (dim, pos));
    knot_eid
}

/// Vanilla `LeadItem.bindPlayerMobs`: move every tether this player holds onto
/// the knot at `(dim, pos)`. Returns false when the player holds nothing —
/// the right-click then falls through to the fence's normal behaviour.
fn hitch_player_tethers(player_eid: i32, dim: DimId, pos: BlockPos) -> bool {
    let held: Vec<i32> = entity_tethers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .filter(|t| t.anchor == TetherAnchor::Entity(player_eid))
        .map(|t| t.target_eid)
        .collect();
    if held.is_empty() {
        return false;
    }
    let knot_eid = ensure_leash_knot(dim, pos);
    for target in held {
        let mut map = entity_tethers().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = map.get_mut(&target) {
            t.anchor = TetherAnchor::FenceKnot {
                dim,
                pos,
                knot_eid,
            };
        }
        drop(map);
        broadcast_seen(target, move |_c| {
            (0x59, set_entity_link_pkt(target, knot_eid))
        });
    }
    true
}

/// `LeashFenceKnotEntity.hurt` / block loss: the knot dies, every tether it
/// held detaches with its lead dropped at the mob, and the trackers get the
/// `0x42` destroy for the knot itself.
fn break_leash_knot(knot_eid: i32) -> bool {
    if leash_knots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&knot_eid)
        .is_none()
    {
        return false;
    }
    let anchored: Vec<i32> = entity_tethers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .filter(|t| matches!(t.anchor, TetherAnchor::FenceKnot { knot_eid: k, .. } if k == knot_eid))
        .map(|t| t.target_eid)
        .collect();
    for target in anchored {
        let drop_at = mob_tether_pos(target).map(|(_, x, y, z)| (x, y, z));
        detach_tether(target, drop_at);
    }
    broadcast_seen_destroy(knot_eid);
    true
}

/// `set_entity_data` (`0x58`) for the two custom-name slots vanilla writes in
/// `Entity`: index 2 `Optional<TextComponent>` (serializer 6 — a boolean then
/// the component tag) and index 3 `CustomNameVisible` (serializer 8 boolean).
/// A bare `TAG_String` is the serialized form vanilla picks for a literal
/// component without style or siblings.
fn mob_custom_name_meta_pkt(eid: i32, name: &str) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, i64::from(eid));
    p.push(2);
    write_varint(&mut p, 6);
    p.push(1);
    p.extend_from_slice(&nbt_string(name));
    p.push(3);
    write_varint(&mut p, 8);
    p.push(1);
    p.push(0xff);
    p
}

fn broadcast_mob_custom_name(eid: i32, name: &str) {
    let pkt = mob_custom_name_meta_pkt(eid, name);
    broadcast_seen(eid, move |_c| (0x58, pkt.clone()));
}

/// Move the tethered mob by `delta` and tell its trackers. The impulse goes
/// through `vel` (vanilla `deltaMovement`) as well so a mob already in a fluid
/// keeps the pull across `mob_fluid_travel_tick`; the land path is
/// displacement-based, so the position is advanced here directly.
fn tether_move(eid: i32, delta: [f64; 3]) {
    let mob = {
        let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
        mobs.iter().find(|m| m.eid == eid).cloned()
    };
    let Some(mob) = mob else { return };
    {
        let mut p = mob.pos.lock().unwrap_or_else(|e| e.into_inner());
        p.0 += delta[0];
        p.1 += delta[1];
        p.2 += delta[2];
    }
    let (x, y, z, yaw, pitch) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
    *mob.vel.lock().unwrap_or_else(|e| e.into_inner()) = delta;
    let grounded = mob.dim == DimId::Overworld && mob_has_support(mob.etype, x, y, z);
    broadcast_seen(eid, move |_c| {
        (0x70, entity_teleport(eid, x, y, z, yaw, pitch, grounded))
    });
}

/// Set the tethered mob's position outright (the non-breaking chain clamp).
fn tether_set_pos(eid: i32, x: f64, y: f64, z: f64) {
    let mob = {
        let mobs = mobs().lock().unwrap_or_else(|e| e.into_inner());
        mobs.iter().find(|m| m.eid == eid).cloned()
    };
    let Some(mob) = mob else { return };
    let (.., yaw, pitch) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
    *mob.pos.lock().unwrap_or_else(|e| e.into_inner()) = (x, y, z, yaw, pitch);
    let grounded = mob.dim == DimId::Overworld && mob_has_support(mob.etype, x, y, z);
    broadcast_seen(eid, move |_c| {
        (0x70, entity_teleport(eid, x, y, z, yaw, pitch, grounded))
    });
}

/// One tether's share of `Entity.tick` (`Leashable.tickLeash`): resolve both
/// ends, snap or clamp past `max_length`, and inside the elastic band apply
/// vanilla's per-axis impulse `copySign(d * d * 0.4, d)` where `d` is the unit
/// direction from target to anchor.
fn tether_step(tether: &EntityTether) {
    let Some((tdim, tx, ty, tz)) = mob_tether_pos(tether.target_eid) else {
        // The target is gone (death/unload): vanilla drops the leash where the
        // holder was, so the lead lands at the anchor rather than nowhere.
        let drop_at = tether_anchor_pos(&tether.anchor).map(|(_, x, y, z)| (x, y, z));
        detach_tether(tether.target_eid, drop_at);
        return;
    };
    let Some((adim, ax, ay, az)) = tether_anchor_pos(&tether.anchor) else {
        // Holder gone without an eager cleanup path: drop the lead at the mob.
        detach_tether(tether.target_eid, Some((tx, ty, tz)));
        return;
    };
    if adim != tdim {
        // Vanilla leashes never cross dimensions; the same snap applies.
        detach_tether(tether.target_eid, Some((tx, ty, tz)));
        return;
    }
    let (dx, dy, dz) = (ax - tx, ay - ty, az - tz);
    let dist = (dx * dx + dy * dy + dz * dz).sqrt();
    if dist > tether.max_length {
        if tether.break_on_stretch {
            detach_tether(tether.target_eid, Some((tx, ty, tz)));
        } else {
            // Non-breaking chain: pull the entity cleanly back onto the
            // max_length sphere, no snap and no item drop.
            let scale = tether.max_length / dist;
            tether_set_pos(
                tether.target_eid,
                ax - dx * scale,
                ay - dy * scale,
                az - dz * scale,
            );
        }
    } else if dist > tether.elastic_start && dist > f64::EPSILON {
        let impulse = |d: f64| 0.4 * d * d * d.signum();
        tether_move(
            tether.target_eid,
            [
                impulse(dx / dist),
                impulse(dy / dist),
                impulse(dz / dist),
            ],
        );
    }
}

/// `LeashFenceKnotEntity.tick -> survives()`: a knot whose fence block is gone
/// dies the same tick — `break_leash_knot` detaches every tether it held and
/// destroys the entity on the wire. Iterated only while knots exist.
fn leash_knot_survival_tick() {
    let knots: Vec<(i32, DimId, BlockPos)> = leash_knots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(&eid, &(dim, pos))| (eid, dim, pos))
        .collect();
    let Some(reg) = crate::block_registry::breg() else {
        return;
    };
    for (eid, dim, (x, y, z)) in knots {
        if !reg.name_of(block_at_dim(dim, x, y, z)).ends_with("_fence") {
            break_leash_knot(eid);
        }
    }
}

/// The tether stage of `game_tick`. An empty table pays one lock and one
/// `is_empty`: zero RNG draws, zero entity scans, and the empty-world digest
/// stays byte-identical.
fn tether_tick() {
    let empty = entity_tethers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
        && leash_knots()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
    if empty {
        return;
    }
    leash_knot_survival_tick();
    let tethers: Vec<EntityTether> = entity_tethers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .copied()
        .collect();
    for tether in &tethers {
        tether_step(tether);
    }
}

fn live_mob_interact(client: &Client, target: i32, slot: i16) -> bool {
    let gm = *client.gm.lock().unwrap();
    if gm == 3 || !client.seen.lock().unwrap().contains(&target) {
        return false;
    }
    let mob = mobs()
        .lock()
        .unwrap()
        .iter()
        .find(|mob| mob.eid == target && matches!(mob.etype, 15 | 78 | 122))
        .cloned();
    let Some(mob) = mob else { return false };
    let mut inv = client.inv.lock().unwrap();
    let item = inv
        .get(&slot)
        .and_then(|stack| item_to_name(stack.0))
        .unwrap_or("");
    let mut pet_update = None;
    let mut tame_status = None;
    let mut pet_hp = None;
    let consumed = {
        let mut states = live_mob_states().lock().unwrap();
        let state = states
            .entry(target)
            .or_insert_with(|| LiveMobState::new(mob.etype));
        match &mut state.behavior {
            Some(MobBehaviorState::Piglin(piglin)) => piglin.offer(client.eid, item),
            Some(MobBehaviorState::Wolf(pet) | MobBehaviorState::Cat(pet)) => {
                let result = pet.try_tame(client.uuid, item, &mut |bound| {
                    (world_rng::next_u64() % u64::from(bound)) as u32
                });
                match result {
                    PetTameResult::Tamed => {
                        pet_hp = Some(pet.hp);
                        pet_update = Some((pet.sitting, true));
                        tame_status = Some(7);
                    }
                    PetTameResult::Consumed => tame_status = Some(6),
                    PetTameResult::Pass => {
                        if let Some(sitting) = pet.toggle_sitting(client.uuid) {
                            pet_update = Some((sitting, true));
                        }
                    }
                }
                result != PetTameResult::Pass
            }
            _ => false,
        }
    };
    if consumed && gm != 1 {
        if let Some(stack) = inv.get_mut(&slot) {
            stack.1 -= 1;
            if stack.1 == 0 {
                inv.remove(&slot);
            }
        }
        let updated = inv.get(&slot).cloned().unwrap_or((0, 0, Vec::new()));
        drop(inv);
        resync_slot(client, 0, slot, &updated);
    } else {
        drop(inv);
    }
    if let Some(hp) = pet_hp {
        *mob.hp.lock().unwrap() = hp;
    }
    if let Some((sitting, tamed)) = pet_update {
        if mob.etype == 15 {
            let mut cats = sitting_cats().lock().unwrap();
            if sitting {
                cats.insert(target);
            } else {
                cats.remove(&target);
            }
        }
        let flags = u8::from(sitting) | (u8::from(tamed) << 2);
        broadcast_seen(target, |_| (0x58, live_mob_metadata(target, 17, 0, flags)));
    }
    if let Some(status) = tame_status {
        let mut packet = target.to_be_bytes().to_vec();
        packet.push(status);
        broadcast_seen(target, |_| (0x1f, packet.clone()));
    }
    consumed || pet_update.is_some()
}

struct LiveMobPlayer<'a> {
    client: &'a Arc<Client>,
    pos: (f64, f64, f64),
    dim: DimId,
    held_item: Option<u32>,
    attackable: bool,
}

impl<'a> LiveMobPlayer<'a> {
    fn new(client: &'a Arc<Client>) -> Self {
        let (x, y, z, ..) = *client.pos.lock().unwrap();
        let slot = 36 + *client.held.lock().unwrap();
        let held_item = client
            .inv
            .lock()
            .unwrap()
            .get(&slot)
            .map(|stack| stack.0 as u32);
        let gm = *client.gm.lock().unwrap();
        Self {
            client,
            pos: (x, y, z),
            dim: *client.dim.lock().unwrap(),
            held_item,
            attackable: matches!(gm, 0 | 2) && *client.hp.lock().unwrap() > 0.0,
        }
    }

    fn perceive(&self) -> AiEntityPerception {
        let mut player = AiEntityPerception::new(self.client.eid, self.pos);
        player.held_item = self.held_item;
        player
    }
}

fn live_mob_block_boxes(
    dim: DimId,
    reg: &crate::block_registry::BReg,
    min: [f64; 3],
    max: [f64; 3],
) -> Vec<([f64; 3], [f64; 3])> {
    if dim == DimId::Overworld {
        return collect_block_collision_boxes(min, max);
    }
    let low = min.map(|value| (value - 1e-7).floor() as i32 - 1);
    let high = max.map(|value| (value + 1e-7).floor() as i32 + 1);
    let mut boxes = Vec::new();
    for x in low[0]..=high[0] {
        for y in low[1]..=high[1] {
            for z in low[2]..=high[2] {
                let offset = [f64::from(x), f64::from(y), f64::from(z)];
                for (lo, hi) in reg.collision_boxes(block_at_dim(dim, x, y, z)) {
                    let world_box = (
                        std::array::from_fn(|i| lo[i] + offset[i]),
                        std::array::from_fn(|i| hi[i] + offset[i]),
                    );
                    if aabb_overlaps((min, max), world_box) {
                        boxes.push(world_box);
                    }
                }
            }
        }
    }
    boxes
}

// Test the actual block shapes along the ray, including partial blocks.
fn live_mob_block_hit(
    dim: DimId,
    reg: &crate::block_registry::BReg,
    from: [f64; 3],
    to: [f64; 3],
) -> Option<[f64; 3]> {
    let delta = mob_behavior_delta(from, to);
    let steps = (mob_behavior_dot(delta, delta).sqrt() / 2.0)
        .ceil()
        .max(1.0) as u32;
    for step in 0..steps {
        let start =
            std::array::from_fn(|i| from[i] + delta[i] * f64::from(step) / f64::from(steps));
        let end =
            std::array::from_fn(|i| from[i] + delta[i] * f64::from(step + 1) / f64::from(steps));
        let lo = std::array::from_fn(|i| start[i].min(end[i]) - 1e-7);
        let hi = std::array::from_fn(|i| start[i].max(end[i]) + 1e-7);
        let distance_sq = |p| {
            let d = mob_behavior_delta(start, p);
            mob_behavior_dot(d, d)
        };
        if let Some(hit) = live_mob_block_boxes(dim, reg, lo, hi)
            .into_iter()
            .filter_map(|(min, max)| {
                let shape = Aabb::new(min, max);
                if shape.contains(start) {
                    Some(start)
                } else {
                    shape.clip(start, end)
                }
            })
            .min_by(|a, b| distance_sq(*a).total_cmp(&distance_sq(*b)))
        {
            return Some(hit);
        }
    }
    None
}

struct LiveSkeletonBowGoal;

impl Goal for LiveSkeletonBowGoal {
    fn priority(&self) -> i32 {
        4
    }
    fn flags(&self) -> u8 {
        GoalFlags::MOVE | GoalFlags::LOOK
    }
    fn can_use(&self, ctx: &GoalContext<'_>) -> bool {
        ctx.target.is_some()
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.can_use(ctx)
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn stop(&mut self, _: &GoalContext<'_>) {}
    fn tick(&mut self, ctx: &GoalContext<'_>) -> Option<GoalOutput> {
        let target = ctx.target?;
        let distance = entity_distance_sq(ctx.position, target.position);
        let move_target = if !target.line_of_sight || distance > 12.0f64.powi(2) {
            Some(target.position)
        } else if distance < 5.0f64.powi(2) {
            let (x, y, z) = ctx.position;
            let (tx, _, tz) = target.position;
            let [dx, _, dz] = mob_behavior_unit([x - tx, 0.0, z - tz]);
            Some((x + 5.0 * dx, y, z + 5.0 * dz))
        } else {
            None
        };
        Some(GoalOutput {
            move_target,
            look_target: Some(target.position),
            attack_target: (target.line_of_sight && distance < 16.0f64.powi(2))
                .then_some(target.entity_id),
            ..GoalOutput::default()
        })
    }
}

// The live passive tick has a rest interval between random walks. Without it
// the standalone GP-1 stroll goal chooses a destination on every idle tick.
struct LivePastureRest {
    ticks: u64,
    first_walk: u64,
}

impl Goal for LivePastureRest {
    fn priority(&self) -> i32 {
        5
    }
    fn flags(&self) -> u8 {
        GoalFlags::MOVE
    }
    fn can_use(&self, _: &GoalContext<'_>) -> bool {
        let interval = MOB_SPAWN_CHANCE_DENOMINATOR / 12;
        self.ticks < self.first_walk || (self.ticks - self.first_walk) % interval >= 10
    }
    fn can_continue_to_use(&self, ctx: &GoalContext<'_>) -> bool {
        self.can_use(ctx)
    }
    fn start(&mut self, _: &GoalContext<'_>) {}
    fn stop(&mut self, _: &GoalContext<'_>) {}
    fn idle_tick(&mut self, _: &GoalContext<'_>) {
        self.ticks += 1;
    }
    fn tick(&mut self, _: &GoalContext<'_>) -> Option<GoalOutput> {
        self.ticks += 1;
        None
    }
}

fn new_live_mob_profile(mob: &Mob) -> MobAiProfile {
    let etype = mob.etype;
    let mut profile = MobAiProfile::new(etype);
    if matches!(etype, 19 | 22 | 77 | 87) {
        let interval = MOB_SPAWN_CHANCE_DENOMINATOR / 12;
        profile.selector.add_goal(Box::new(LivePastureRest {
            ticks: 0,
            first_walk: interval + mob.eid as u64 % interval,
        }));
    }
    if etype == 91 {
        profile.selector = MobGoalPlan::for_entity(
            etype,
            MobGoalSettings {
                skeleton_has_bow: true,
                ..MobGoalSettings::default()
            },
        )
        .unwrap()
        .build_selector(etype as u64);
        profile
            .selector
            .add_goal(Box::new(FloatGoal { priority: 0 }));
        profile.selector.add_goal(Box::new(LiveSkeletonBowGoal));
        profile.movement_speed = 0.22;
    }
    profile
}

fn live_mob_move(
    mob: &Arc<Mob>,
    waypoint_y: Option<f64>,
    dim: DimId,
    reg: &crate::block_registry::BReg,
    result: MobAiStepResult,
    in_fluid: bool,
) {
    let mut pos = mob.pos.lock().unwrap();
    let old = *pos;
    let (dx, _, dz) = result.movement_delta;
    let (width, height) = entity_dimensions_1211(mob.etype);
    let collides = |point: [f64; 3], delta, step| {
        let bounds = EntityAabb::from_dimensions(point[0], point[1], point[2], width, height);
        entity_collide(bounds, delta, true, step, &[], |sweep| {
            live_mob_block_boxes(dim, reg, sweep.min(), sweep.max())
        })
    };
    let flying = matches!(mob.etype, 6 | 8 | 31 | 45 | 119);
    if !flying && !in_fluid {
        let mut jumped = false;
        if let Some(next_y) = waypoint_y.filter(|_| result.jump) {
            let rise = next_y - pos.1;
            if rise > 0.5 && rise <= 1.25 {
                let start = [pos.0, pos.1, pos.2];
                let step = collides(start, [dx, -1e-4, dz], rise as f32 + 1e-4);
                if step.stepped_up && step.delta[1] > 0.5 {
                    pos.0 += step.delta[0];
                    pos.1 += step.delta[1];
                    pos.2 += step.delta[2];
                    jumped = true;
                }
            }
        }
        if !jumped && dim == DimId::Overworld {
            if dx != 0.0 || dz != 0.0 {
                mob_collide_move(mob, &mut pos, dx, dz);
            } else if let Some((landing_pos, fall)) = mob_snap_y(mob, &mut pos) {
                drop(pos);
                let landed = landing::cow_land(mob, landing_pos, fall);
                pos = mob.pos.lock().unwrap();
                if landed {
                    pos.1 += 1.0 / 16.0;
                }
            }
        } else if !jumped {
            let step = collides([pos.0, pos.1, pos.2], [dx, -0.25, dz], 0.6);
            pos.0 += step.delta[0];
            pos.1 += step.delta[1];
            pos.2 += step.delta[2];
        }
    }
    if let Some(yaw) = result.look_yaw {
        pos.3 = yaw;
    }
    if let Some(pitch) = result.look_pitch {
        pos.4 = pitch;
    }
    let (x, y, z, yaw, pitch) = *pos;
    drop(pos);
    if old != (x, y, z, yaw, pitch) {
        let grounded = !flying
            && if dim == DimId::Overworld {
                mob_has_support(mob.etype, x, y, z)
            } else {
                collides([x, y, z], [0.0, -1e-4, 0.0], 0.0).vertical_collision_below
            };
        broadcast_seen(mob.eid, |_| {
            (
                0x70,
                entity_teleport(mob.eid, x, y, z, yaw, pitch, grounded),
            )
        });
        if old.3 != yaw {
            broadcast_seen(mob.eid, |_| (0x48, head_rot(mob.eid, yaw)));
        }
    }
}

fn live_skeleton_shoot(mob: &Mob, pos: [f64; 3], target: (f64, f64, f64)) {
    let mut last_hit = mob.last_hit.lock().unwrap();
    if last_hit.elapsed() < Duration::from_secs(3) {
        return;
    }
    *last_hit = Instant::now();
    let [x, y, z] = [pos[0], pos[1] + 1.6, pos[2]];
    let [dx, dy, dz] = mob_behavior_unit([target.0 - x, target.1 + 1.2 - y, target.2 - z]);
    arrows().lock().unwrap().push(Arrow {
        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
        owner: -1,
        x,
        y,
        z,
        dx,
        dy,
        dz,
        ttl: server_ticks(MOB_ARROW_TTL_GT) as i32,
        dmg: diff_mob_dmg(3.0),
        fire: 0,
        punch: 0,
        kind: 0,
        loy: 0,
        ret_item: 0,
        ret_comps: Vec::new(),
        pierce: 0,
        last_hit: -1,
        stuck: false,
    });
}

fn live_mob_metadata(eid: i32, index: u8, kind: u8, value: u8) -> Vec<u8> {
    let mut packet = Vec::new();
    write_varint(&mut packet, i64::from(eid));
    packet.extend([index, kind, value, 0xff]);
    packet
}

fn live_mob_spawn_packets(mob: &Mob) -> Vec<(i64, Vec<u8>)> {
    let mut packets = vec![(0x01, spawn_mob(mob))];
    if let Some(name) = mob_custom_names()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&mob.eid)
    {
        packets.push((0x58, mob_custom_name_meta_pkt(mob.eid, name)));
    }
    if let Some(tether) = entity_tethers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&mob.eid)
    {
        let holding = match tether.anchor {
            TetherAnchor::Entity(eid) => eid,
            TetherAnchor::FenceKnot { knot_eid, .. } => knot_eid,
        };
        packets.push((0x59, set_entity_link_pkt(mob.eid, holding)));
    }
    if let Some(state) = live_mob_states().lock().unwrap().get(&mob.eid) {
        match &state.behavior {
            Some(MobBehaviorState::Slime { behavior, .. }) => {
                packets.push((0x58, live_mob_metadata(mob.eid, 16, 1, behavior.size as u8)));
            }
            Some(MobBehaviorState::Blaze(_)) if state.blaze_charged => {
                packets.push((0x58, live_mob_metadata(mob.eid, 16, 0, 1)));
            }
            Some(MobBehaviorState::Wolf(pet) | MobBehaviorState::Cat(pet)) => {
                let flags = u8::from(pet.sitting) | (u8::from(pet.owner.is_some()) << 2);
                packets.push((0x58, live_mob_metadata(mob.eid, 17, 0, flags)));
            }
            _ => {}
        }
        let now = TICK.load(Ordering::Relaxed);
        for &(potion, until) in &state.potions {
            if until > now {
                packets.push((
                    0x76,
                    entity_effect_pkt(
                        mob.eid,
                        live_witch_effect_id(potion),
                        0,
                        (until - now) as i32,
                    ),
                ));
            }
        }
    }
    packets
}

#[derive(Clone, Copy)]
struct LiveMobProjectile {
    eid: i32,
    owner: i32,
    dim: DimId,
    kind: MobProjectileKind,
    pos: [f64; 3],
    velocity: [f64; 3],
    power: f32,
    age: u16,
}

impl LiveMobProjectile {
    fn entity_type(self) -> i64 {
        let name = match self.kind {
            MobProjectileKind::SmallFireball => "small_fireball",
            MobProjectileKind::GhastFireball => "fireball",
            MobProjectileKind::ShulkerBullet { .. } => "shulker_bullet",
            MobProjectileKind::DragonFireball => "dragon_fireball",
            MobProjectileKind::WitchSplash(_) => "potion",
            MobProjectileKind::WitherSkull { .. } => "wither_skull",
        };
        name_to_etype(name).expect("behavior projectile type in protocol registry")
    }
}

fn live_mob_projectiles() -> &'static Mutex<Vec<LiveMobProjectile>> {
    static PROJECTILES: OnceLock<Mutex<Vec<LiveMobProjectile>>> = OnceLock::new();
    PROJECTILES.get_or_init(Mutex::default)
}

fn live_mob_projectile_packets(projectile: LiveMobProjectile) -> Vec<(i64, Vec<u8>)> {
    let [x, y, z] = projectile.pos;
    let [vx, vy, vz] = projectile.velocity.map(|axis| (axis * 8000.0) as i16);
    let mut spawn = spawn_entity_pkt(
        projectile.eid,
        projectile.entity_type() as u32,
        x,
        y,
        z,
        0.0,
        0.0,
        i64::from(projectile.owner),
    );
    let offset = spawn.len() - 6;
    spawn[offset..]
        .copy_from_slice(&[vx.to_be_bytes(), vy.to_be_bytes(), vz.to_be_bytes()].concat());
    let mut packets = vec![(0x01, spawn)];
    if matches!(
        projectile.kind,
        MobProjectileKind::WitherSkull { blue: true }
    ) {
        packets.push((0x58, live_mob_metadata(projectile.eid, 8, 8, 1)));
    }
    packets
}

fn live_mob_projectile_impact(
    projectile: LiveMobProjectile,
    pos: [f64; 3],
    hit: Option<&LiveMobPlayer<'_>>,
    players: &[LiveMobPlayer<'_>],
) {
    use MobProjectileKind::*;
    match projectile.kind {
        SmallFireball => {
            if let Some(player) = hit {
                live_mob_damage_player(player.client, pos, diff_mob_dmg(5.0), Some(3), None);
            } else if GR_MOB_GRIEFING.load(Ordering::Relaxed) != 0 {
                let p = mob_behavior_delta(projectile.velocity.map(|axis| axis * 1e-4), pos);
                let [x, y, z] = p.map(|axis| axis.floor() as i32);
                if block_at_dim(projectile.dim, x, y, z) == 0 {
                    place_block_dim(projectile.dim, x, y, z, name_to_state("fire").unwrap());
                }
            }
        }
        GhastFireball => {
            if let Some(player) = hit {
                live_mob_damage_player(player.client, pos, diff_mob_dmg(6.0), Some(3), None);
            }
            w1_live_mob_explosion(projectile.dim, pos, projectile.power, players);
        }
        WitherSkull { .. } => {
            if let Some(player) = hit {
                if live_mob_damage_player(player.client, pos, diff_mob_dmg(8.0), Some(3), None) {
                    let ticks = match DIFFICULTY.load(Ordering::Relaxed) {
                        2 => 200,
                        3 => 800,
                        _ => 0,
                    };
                    if ticks > 0 {
                        live_mob_player_effect(player.client, 19, ticks);
                    }
                }
            }
            w1_live_mob_explosion(projectile.dim, pos, projectile.power, players);
        }
        ShulkerBullet { .. } => {
            if let Some(player) = hit {
                if live_mob_damage_player(player.client, pos, diff_mob_dmg(4.0), Some(3), None) {
                    live_mob_player_effect(player.client, 24, 200);
                }
            }
        }
        WitchSplash(potion) => {
            for player in players
                .iter()
                .filter(|player| player.dim == projectile.dim && player.attackable)
            {
                let distance = entity_distance_sq((pos[0], pos[1], pos[2]), player.pos).sqrt();
                let scale = if hit.is_some_and(|hit| hit.client.eid == player.client.eid) {
                    1.0
                } else {
                    (1.0 - distance / 4.0).max(0.0)
                };
                let ticks = match potion {
                    WitchPotion::Slowness | WitchPotion::Weakness => 1800,
                    WitchPotion::Poison => 900,
                    _ => 0,
                };
                if potion == WitchPotion::Harming {
                    live_mob_damage_player(player.client, pos, (6.0 * scale) as f32, None, None);
                } else if scale * f64::from(ticks) > 20.0 {
                    live_mob_player_effect(
                        player.client,
                        live_witch_effect_id(potion),
                        (f64::from(ticks) * scale) as u16,
                    );
                }
            }
        }
        DragonFireball => {
            // The breath cloud lingers after its projectile is gone.
            live_dragon_breath(projectile.dim, pos);
        }
    }
}

struct LiveDragonBreath {
    eid: i32,
    dim: DimId,
    pos: [f64; 3],
    left: u16,
}

fn live_dragon_clouds() -> &'static Mutex<Vec<LiveDragonBreath>> {
    static CLOUDS: OnceLock<Mutex<Vec<LiveDragonBreath>>> = OnceLock::new();
    CLOUDS.get_or_init(Mutex::default)
}

fn live_dragon_breath(dim: DimId, pos: [f64; 3]) {
    let mut clouds = live_dragon_clouds().lock().unwrap();
    if clouds.len() < MOB_CAP {
        clouds.push(LiveDragonBreath {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            dim,
            pos,
            left: 200,
        });
    }
}

fn tick_live_mob_projectiles(reg: &crate::block_registry::BReg, players: &[LiveMobPlayer<'_>]) {
    let mut projectiles = live_mob_projectiles().lock().unwrap();
    projectiles.retain_mut(|projectile| {
        let LiveMobProjectile { eid, dim, pos, .. } = *projectile;
        projectile.age += 1;
        if projectile.age > 200 {
            broadcast_seen_destroy(eid);
            return false;
        }
        if let MobProjectileKind::ShulkerBullet { target_eid } = projectile.kind {
            if let Some(target) = players
                .iter()
                .find(|p| p.client.eid == target_eid && p.dim == dim)
            {
                let direction = mob_behavior_unit(mob_behavior_delta(
                    pos,
                    [target.pos.0, target.pos.1 + 0.9, target.pos.2],
                ));
                projectile.velocity =
                    std::array::from_fn(|i| 0.7 * projectile.velocity[i] + 0.09 * direction[i]);
            }
        }
        let next = std::array::from_fn(|i| pos[i] + projectile.velocity[i]);
        let block = live_mob_block_hit(dim, reg, pos, next);
        let end = block.unwrap_or(next);
        let distance = |point| {
            let d = mob_behavior_delta(pos, point);
            mob_behavior_dot(d, d)
        };
        let hit = players
            .iter()
            .filter(|p| p.dim == dim && p.attackable)
            .filter_map(|player| {
                let (x, y, z) = player.pos;
                let bounds = Aabb::new([x - 0.4, y - 0.1, z - 0.4], [x + 0.4, y + 1.9, z + 0.4]);
                let point = if bounds.contains(pos) {
                    Some(pos)
                } else {
                    bounds.clip(pos, end)
                }?;
                Some((player, point))
            })
            .min_by(|(_, a), (_, b)| distance(*a).total_cmp(&distance(*b)));
        if let Some((player, point)) = hit {
            live_mob_projectile_impact(*projectile, point, Some(player), players);
        } else if let Some(point) = block {
            // P6c / #215: the hit cell is one nudge past the surface point
            // along the shot; a `target` block there takes its
            // onProjectileHit hook.
            let cell: [i32; 3] = std::array::from_fn(|i| {
                (point[i] + projectile.velocity[i] * 1e-3).floor() as i32
            });
            w1_live_target_projectile_hit(
                dim,
                RsPos(cell[0], cell[1], cell[2]),
                point,
                w1_dominant_axis(projectile.velocity),
                false,
            );
            live_mob_projectile_impact(*projectile, point, None, players);
        } else {
            projectile.pos = next;
            if matches!(projectile.kind, MobProjectileKind::WitchSplash(_)) {
                projectile.velocity[1] -= 0.05;
            }
            let [x, y, z] = next;
            broadcast_seen(eid, |_| {
                (0x70, entity_teleport(eid, x, y, z, 0.0, 0.0, false))
            });
            return true;
        }
        broadcast_seen_destroy(eid);
        false
    });
    drop(projectiles);
    live_dragon_clouds().lock().unwrap().retain_mut(|cloud| {
        cloud.left -= 1;
        if cloud.left == 0 {
            broadcast_seen_destroy(cloud.eid);
            return false;
        }
        if cloud.left.is_multiple_of(20) {
            let [x, y, z] = cloud.pos;
            for player in players
                .iter()
                .filter(|p| p.dim == cloud.dim && entity_distance_sq(p.pos, (x, y, z)) < 9.0)
            {
                live_mob_damage_player(player.client, cloud.pos, 6.0, None, None);
            }
        }
        true
    });
}

fn live_mob_behavior_step(
    mob: &Arc<Mob>,
    reg: &crate::block_registry::BReg,
    mut ctx: MobBehaviorTickCtx,
    target: Option<&LiveMobPlayer<'_>>,
    crystals: &[DragonCrystal],
) -> Vec<MobBehaviorEffect> {
    let mut states = live_mob_states().lock().unwrap();
    let state = states
        .entry(mob.eid)
        .or_insert_with(|| LiveMobState::new(mob.etype));
    let Some(behavior) = &mut state.behavior else {
        return Vec::new();
    };
    let tick = TICK.load(Ordering::Relaxed);
    state.potions.retain(|&(_, expiry)| expiry > tick);
    let mut block_at = |[x, y, z]: [i32; 3]| {
        let id = block_at_dim(ctx.dim, x, y, z);
        EndermanTeleportBlock {
            blocks_motion: reg.blocks_motion(id),
            liquid: reg.fluid_name(id) != "minecraft:empty",
        }
    };
    let mut can_occupy = |[x, y, z]: [f64; 3]| {
        let (width, height) = entity_dimensions_1211(mob.etype);
        let bounds = EntityAabb::from_dimensions(x, y, z, width, height);
        live_mob_block_boxes(ctx.dim, reg, bounds.min(), bounds.max()).is_empty()
    };
    let active_effects: Vec<_> = state.potions.iter().map(|&(effect, _)| effect).collect();
    let target_effects = if matches!(behavior, MobBehaviorState::Witch(_)) {
        target.map_or_else(Vec::new, |target| {
            let active = target.client.effs.lock().unwrap();
            [
                WitchPotion::Slowness,
                WitchPotion::Poison,
                WitchPotion::Weakness,
            ]
            .into_iter()
            .filter(|potion| {
                active
                    .iter()
                    .any(|eff| eff.id == live_witch_effect_id(*potion))
            })
            .collect()
        })
    } else {
        Vec::new()
    };
    let shot_ready = || mob.last_hit.lock().unwrap().elapsed() >= Duration::from_secs(2);
    let world = match behavior {
        MobBehaviorState::Blaze(_) => {
            ctx.on_fire = state.blaze_charged;
            MobBehaviorWorldTick::Basic
        }
        MobBehaviorState::Enderman => MobBehaviorWorldTick::Enderman {
            block_at: &mut block_at,
            can_occupy: &mut can_occupy,
        },
        MobBehaviorState::Witch { .. } => MobBehaviorWorldTick::Witch {
            eid: mob.eid,
            active_effects: &active_effects,
            target_effects: &target_effects,
            throw_ready: shot_ready(),
        },
        MobBehaviorState::Wither(_) if shot_ready() => {
            MobBehaviorWorldTick::WitherShot { force_blue: false }
        }
        MobBehaviorState::EnderDragon(dragon) => {
            let portal_y = live_dragon_portal_y(ctx.dim);
            let path_complete = match dragon.phase {
                DragonPhase::HoldingPattern | DragonPhase::Takeoff => dragon.phase_ticks >= 60,
                DragonPhase::LandingApproach => ctx.pos[0].hypot(ctx.pos[2]) <= 1.0,
                _ => false,
            };
            MobBehaviorWorldTick::EnderDragon {
                portal_y,
                path_complete,
                strafe_aligned: ctx.nearest_target.is_some_and(|target| target.visible),
                crystals,
            }
        }
        _ => MobBehaviorWorldTick::Basic,
    };
    let mut effects = behavior.step_with_world(&ctx, world);
    if matches!(
        behavior,
        MobBehaviorState::Witch(_) | MobBehaviorState::Wither(_)
    ) && effects
        .iter()
        .any(|effect| matches!(effect, MobBehaviorEffect::LaunchProjectile { .. }))
    {
        *mob.last_hit.lock().unwrap() = Instant::now();
    }
    if let MobBehaviorState::EnderDragon(dragon) = behavior {
        if ctx.dim == DimId::End {
            let perch = EnderDragonBehavior::perch(live_dragon_portal_y(ctx.dim));
            let waypoint = match dragon.phase {
                DragonPhase::LandingApproach => Some([perch[0], perch[1] + 12.0, perch[2]]),
                DragonPhase::Landing | DragonPhase::Dying => Some(perch),
                DragonPhase::HoldingPattern | DragonPhase::Takeoff => {
                    let angle = f64::from(dragon.phase_ticks) * 0.035;
                    Some([24.0 * angle.cos(), perch[1] + 16.0, 24.0 * angle.sin()])
                }
                DragonPhase::StrafePlayer => ctx
                    .nearest_target
                    .map(|target| [target.pos[0], target.pos[1] + 8.0, target.pos[2]]),
                _ => None,
            };
            if let Some(waypoint) = waypoint {
                let delta = mob_behavior_delta(ctx.pos, waypoint);
                let length = mob_behavior_dot(delta, delta).sqrt();
                if length > 0.05 {
                    effects.push(MobBehaviorEffect::Teleport {
                        to: std::array::from_fn(|i| ctx.pos[i] + delta[i] / length.max(1.0)),
                    });
                }
            }
        }
    }
    effects
}

fn live_dragon_portal_y(dim: DimId) -> i32 {
    // The centre post of an active podium rises four blocks from the portal.
    let surface = height_at_dim(dim, 0, 0);
    if block_at_dim(dim, 0, surface - 1, 0) == name_to_state("bedrock").unwrap() {
        surface - 4
    } else {
        surface
    }
}

fn apply_live_mob_effects(
    mob: &Arc<Mob>,
    dim: DimId,
    pos: [f64; 3],
    effects: Vec<MobBehaviorEffect>,
    players: &[LiveMobPlayer<'_>],
) -> bool {
    use MobBehaviorEffect::*;
    let player = |eid| players.iter().find(|p| p.client.eid == eid && p.dim == dim);
    for effect in effects {
        match effect {
            SetOnFire(charged) => {
                if let Some(state) = live_mob_states().lock().unwrap().get_mut(&mob.eid) {
                    state.blaze_charged = charged;
                }
                broadcast_seen(mob.eid, |_| {
                    (0x58, live_mob_metadata(mob.eid, 16, 0, u8::from(charged)))
                });
            }
            LaunchProjectile {
                kind,
                mut origin,
                dir,
                power,
            } => {
                // The behavior aims from feet to feet. Shift the shot to body height on both ends.
                origin[1] += if mob.etype == 31 { 2.0 } else { 1.0 };
                let speed = match kind {
                    MobProjectileKind::WitchSplash(_) => 0.75,
                    MobProjectileKind::ShulkerBullet { .. } => 0.15,
                    _ => 0.5,
                };
                let mut dir = mob_behavior_unit(dir);
                if matches!(kind, MobProjectileKind::WitchSplash(_)) {
                    dir[1] += 0.25;
                }
                let mut projectiles = live_mob_projectiles().lock().unwrap();
                if projectiles.len() < MOB_CAP {
                    projectiles.push(LiveMobProjectile {
                        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                        owner: mob.eid,
                        dim,
                        kind,
                        pos: origin,
                        velocity: dir.map(|axis| axis * speed),
                        power,
                        age: 0,
                    });
                }
            }
            MeleeDamage {
                target_eid,
                amount,
                knockback_y,
            } => {
                if let Some(player) = player(target_eid) {
                    live_mob_melee(mob, player.client, pos, amount, knockback_y.max(0.35));
                }
            }
            SonicBoom { target_eid, damage } => {
                if let Some(player) = player(target_eid) {
                    live_mob_damage_player(
                        player.client,
                        pos,
                        diff_mob_dmg(damage),
                        None,
                        Some(0.5),
                    );
                }
            }
            Explosion { pos, power } => w1_live_mob_explosion(dim, pos, power, players),
            Teleport { to: [x, y, z] } => {
                let (yaw, pitch) = {
                    let mut position = mob.pos.lock().unwrap();
                    *position = (x, y, z, position.3, position.4);
                    (position.3, position.4)
                };
                mob.vel.lock().unwrap().fill(0.0);
                broadcast_seen(mob.eid, |_| {
                    (
                        0x70,
                        entity_teleport(mob.eid, x, y, z, yaw, pitch, mob.etype == 33),
                    )
                });
            }
            ConvertEntity { target_etype } => {
                convert_live_mob(mob, dim, target_etype);
                return false;
            }
            DropItem { item, count } => {
                if let Some(id) =
                    name_to_item(item).filter(|_| GR_MOB_LOOT.load(Ordering::Relaxed) != 0)
                {
                    drop_live_stack((pos[0], pos[1], pos[2]), (id, count, Vec::new()));
                }
            }
            SpawnSplitChildren { etype, size, count } => {
                let hp = f32::from(size as u8).powi(2);
                let offset = f64::from(size as u8) * 0.26;
                for i in 0..count {
                    let dx = if i % 2 == 0 { -offset } else { offset };
                    let dz = if i < 2 { -offset } else { offset };
                    insert_live_mob(
                        etype,
                        dim,
                        (pos[0] + dx, pos[1] + 0.5, pos[2] + dz, 0.0, 0.0),
                        hp,
                        Some(size),
                    );
                }
            }
            ApplyPotion {
                target_eid,
                effect,
                duration_ticks,
            } => {
                if target_eid == mob.eid {
                    let ticks = server_ticks(u64::from(duration_ticks));
                    if let Some(state) = live_mob_states().lock().unwrap().get_mut(&mob.eid) {
                        state.potions.retain(|&(old, _)| old != effect);
                        state
                            .potions
                            .push((effect, TICK.load(Ordering::Relaxed) + ticks));
                    }
                    broadcast_seen(mob.eid, |_| {
                        (
                            0x76,
                            entity_effect_pkt(
                                mob.eid,
                                live_witch_effect_id(effect),
                                0,
                                ticks as i32,
                            ),
                        )
                    });
                } else if let Some(player) = player(target_eid) {
                    live_mob_player_effect(
                        player.client,
                        live_witch_effect_id(effect),
                        duration_ticks,
                    );
                }
            }
            HealSelf(amount) => {
                let mut hp = mob.hp.lock().unwrap();
                if *hp > 0.0 {
                    let max_hp = entity_spec_by_id(mob.etype).unwrap().max_health;
                    *hp = (*hp + amount).min(max_hp);
                }
            }
            OpenEndExitPortal { spawn_egg, xp } if dim == DimId::End => {
                let y = live_dragon_portal_y(dim);
                let portal = name_to_state("end_portal").unwrap();
                let first = block_at_dim(dim, 1, y, 0) != portal;
                let rewards = DragonFightRewards {
                    exit_portal_base: [0, y, 0],
                    dragon_egg: (first && spawn_egg).then_some([0, y + 4, 0]),
                    xp: if first { xp } else { 500 },
                };
                rewards.place_bedrock_exit_portal(|[x, y, z], name| {
                    if let Some(state) = name_to_state(name) {
                        set_block_dim(dim, x, y, z, state);
                        note_block_update_dim(dim, x, y, z, state);
                    }
                });
                xps().lock().unwrap().push(Xp {
                    eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                    x: 0.5,
                    y: f64::from(y + 1),
                    z: 0.5,
                    val: rewards.xp as i16,
                    age: 0,
                });
            }
            OpenEndExitPortal { .. } => {}
            WarningSignal => {
                let status = match mob.etype {
                    116 => Some(62), // Warden sonic charge
                    _ => None,
                };
                if let Some(status) = status {
                    let mut packet = mob.eid.to_be_bytes().to_vec();
                    packet.push(status);
                    broadcast_seen(mob.eid, |_| (0x1f, packet.clone()));
                } else {
                    let packet = sound_pkt_named("entity.ghast.warn", pos[0], pos[1], pos[2]);
                    broadcast_seen(mob.eid, |_| (0x68, packet.clone()));
                }
            }
            PiglinBarter { player_eid, loot } => {
                if let Some(id) = name_to_item(loot.item) {
                    let comps = if let Some(potion) = loot.potion {
                        parse_item_comps(&format!("{}[potion_contents={}]", loot.item, potion))
                    } else if let Some(level) = loot.soul_speed {
                        let component = if loot.item == "enchanted_book" {
                            COMP_STORED_ENCHANTMENTS
                        } else {
                            COMP_ENCHANTMENTS
                        };
                        vec![(
                            component,
                            enchantments_data(&[(35, i64::from(level))], true),
                        )]
                    } else {
                        Vec::new()
                    };
                    let mut drops = drops().lock().unwrap();
                    if drops.len() < DROP_CAP {
                        drops.push(Drop {
                            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                            x: pos[0],
                            y: pos[1] + 0.5,
                            z: pos[2],
                            item: id,
                            cnt: loot.count,
                            comps,
                            age: 0,
                            target: player(player_eid).map(|p| p.client.uuid),
                            thrower: None,
                        });
                    }
                }
            }
            DragonFlameBreath => live_dragon_breath(dim, pos),
            DragonPerchAttack => {
                for player in players.iter().filter(|p| {
                    p.dim == dim && entity_distance_sq(p.pos, (pos[0], pos[1], pos[2])) <= 16.0
                }) {
                    live_mob_melee(mob, player.client, pos, 10.0, 0.5);
                }
            }
        }
    }
    true
}

// ---------- W1 / P6a: phantom, pillager, evoker, guardian live dispatch ----------
//
// `MobBehaviorState` is declared inside `mob_behaviors.rs`, which another
// lane owns, so the four families it does not enumerate keep their behavior
// structs in this parallel map. The live `mob_tick` steps them on the same
// path as the enum-dispatched families, and their attacks replace the
// generic melee fallback at the bottom of `live_mob_tick_dimension`.

enum W1LiveMobAi {
    Phantom(PhantomBehavior),
    Pillager(PillagerBehavior),
    Evoker(EvokerBehavior),
    Guardian(GuardianBehavior),
}

fn w1_live_mob_ai() -> &'static Mutex<HashMap<i32, W1LiveMobAi>> {
    static AI: OnceLock<Mutex<HashMap<i32, W1LiveMobAi>>> = OnceLock::new();
    AI.get_or_init(Mutex::default)
}

/// elder guardian 29, evoker 35, guardian 50, phantom 76, pillager 80.
fn w1_hostile_ai_owns(etype: i64) -> bool {
    matches!(etype, 29 | 35 | 50 | 76 | 80)
}

fn w1_live_mob_ai_for(etype: i64) -> W1LiveMobAi {
    match etype {
        29 => W1LiveMobAi::Guardian(GuardianBehavior::new(true)),
        35 => W1LiveMobAi::Evoker(EvokerBehavior::new()),
        50 => W1LiveMobAi::Guardian(GuardianBehavior::new(false)),
        76 => W1LiveMobAi::Phantom(PhantomBehavior::new()),
        _ => W1LiveMobAi::Pillager(PillagerBehavior::new()),
    }
}

/// Evoker fangs the live dispatch spawned: `(eid, ticks_left)`. Vanilla
/// EvokerFangs lives ~22 ticks; the damage lands on the cast tick.
fn w1_evoker_fangs() -> &'static Mutex<Vec<(i32, i32)>> {
    static FANGS: OnceLock<Mutex<Vec<(i32, i32)>>> = OnceLock::new();
    FANGS.get_or_init(Mutex::default)
}

/// Wool colors the live wololo wrote: mob eid -> dye index. Sheep colors are
/// not instanced at spawn yet, so an unrecorded sheep still counts as a blue
/// candidate for the easter-egg spell.
fn w1_wool_colors() -> &'static Mutex<HashMap<i32, u8>> {
    static WOOL: OnceLock<Mutex<HashMap<i32, u8>>> = OnceLock::new();
    WOOL.get_or_init(Mutex::default)
}

/// Count down live evoker-fang lifetimes each `mob_tick`; a spent fang sends
/// its destroy packet and leaves the tracker list.
fn w1_evoker_fangs_tick() {
    let mut dead = Vec::new();
    w1_evoker_fangs().lock().unwrap_or_else(|e| e.into_inner()).retain_mut(|(eid, left)| {
        *left -= 1;
        if *left <= 0 {
            dead.push(*eid);
        }
        *left > 0
    });
    for eid in dead {
        broadcast_seen_destroy(eid);
    }
}

// ---------- W1 / P2: powder-snow freeze ramp + explosion drop decay ----------

/// Per-player powder-snow contact time in server ticks (`Entity#getTicksFrozen`
/// is not instanced on `Client` yet, so the live ramp is tracked here).
fn w1_frozen_ticks() -> &'static Mutex<HashMap<i32, u64>> {
    static FROZEN: OnceLock<Mutex<HashMap<i32, u64>>> = OnceLock::new();
    FROZEN.get_or_init(Mutex::default)
}

thread_local! {
    /// `(kind, power)` while a live explosion pass destroys blocks: the
    /// `*ExplosionDropDecay` rules gate the spill each broken container emits.
    static W1_EXPLOSION_CTX: std::cell::Cell<Option<(ExplosionDropKind, f32)>> =
        const { std::cell::Cell::new(None) };
}

fn w1_explosion_scope<R>(kind: ExplosionDropKind, power: f32, work: impl FnOnce() -> R) -> R {
    W1_EXPLOSION_CTX.with(|ctx| ctx.set(Some((kind, power))));
    let out = work();
    W1_EXPLOSION_CTX.with(|ctx| ctx.set(None));
    out
}

/// One decay roll per block a live explosion destroys; the counter is the
/// execution evidence the wire tests assert on.
static W1_EXPLOSION_DROP_ROLLS: AtomicU64 = AtomicU64::new(0);

/// The `DESTROY_WITH_DECAY` roll for one explosion-broken block. Outside an
/// explosion context nothing changes (`DESTROY` always drops).
fn w1_explosion_drop_allowed() -> bool {
    let Some((kind, power)) = W1_EXPLOSION_CTX.with(|ctx| ctx.get()) else {
        return true;
    };
    W1_EXPLOSION_DROP_ROLLS.fetch_add(1, Ordering::Relaxed);
    let roll = (world_rng::next_u64() % 1_000_000) as f32 / 1_000_000.0;
    eval_explosion_block_drop(explosion_drop_decay_enabled(kind), power, roll)
}

/// `primed_tick` is the only `explode_at` caller, so the whole TNT phase runs
/// under the `Tnt` `BlockInteraction` at vanilla power 4.
fn w1_tnt_tick() {
    w1_explosion_scope(ExplosionDropKind::Tnt, 4.0, primed_tick);
}

/// Mob-sourced explosions (creepers, ghast/wither/dragon projectiles and the
/// behavior effect) run under `mobExplosionDropDecay`.
fn w1_live_mob_explosion(dim: DimId, pos: [f64; 3], power: f32, players: &[LiveMobPlayer<'_>]) {
    w1_explosion_scope(ExplosionDropKind::Mob, power, || {
        live_mob_explosion(dim, pos, power, players);
    });
}

// ---------- W1 / P6c: canonical target-block projectile hits ----------

fn w1_dominant_axis(velocity: [f64; 3]) -> RsAxis {
    if velocity[0].abs() >= velocity[1].abs() && velocity[0].abs() >= velocity[2].abs() {
        RsAxis::X
    } else if velocity[1].abs() >= velocity[2].abs() {
        RsAxis::Y
    } else {
        RsAxis::Z
    }
}

/// A live projectile hitting a `target` block runs the canonical
/// `redstone::rs_target_projectile_hit` through the same `RedstoneWorldAdapter`
/// `run_redstone_pass` uses, then commits the power write; the power-off tick
/// rides the shared `live_redstone_tick` scheduler.
fn w1_live_target_projectile_hit(
    dim: DimId,
    pos: RsPos,
    hit: [f64; 3],
    face_axis: RsAxis,
    arrow: bool,
) {
    let Some(reg) = breg() else { return };
    let (writes, dispensers) = {
        let mut dimensions = live_redstone().lock().unwrap_or_else(|e| e.into_inner());
        let state = dimensions.entry(dim).or_default();
        let mut torches = std::mem::take(&mut state.torches);
        let now = TICK_IDX.load(Ordering::Relaxed);
        let mut world = LiveRedstoneWorld {
            dim,
            reg,
            state,
            now,
            reads: Default::default(),
            writes: Default::default(),
            dispensers: Vec::new(),
        };
        {
            let mut adapter = RedstoneWorldAdapter::new(reg, &mut world, &mut torches);
            rs_target_projectile_hit(&mut adapter, pos, hit, face_axis, arrow);
            adapter.propagate(&[pos]);
        }
        world.state.torches = torches;
        (world.writes, world.dispensers)
    };
    for (RsPos(x, y, z), state) in writes {
        set_block_if_inner_dim(dim, x, y, z, state, None);
        note_block_update_dim(dim, x, y, z, block_at_dim(dim, x, y, z));
    }
    if dim == DimId::Overworld {
        for RsPos(x, y, z) in dispensers {
            fire_dispenser(x, y, z, block_at(x, y, z));
        }
    }
}

/// `arrow_tick` owns the flight; this wrapper diffs the live arrow list for
/// projectiles that landed inside a `target` block this tick and runs the
/// vanilla `TargetBlock.onProjectileHit` hook on them (P6c / #215).
fn w1_arrow_tick() {
    let before: HashMap<i32, ([f64; 6], bool)> = arrows()
        .lock()
        .unwrap()
        .iter()
        .map(|a| (a.eid, ([a.x, a.y, a.z, a.dx, a.dy, a.dz], a.stuck)))
        .collect();
    arrow_tick();
    let Some(reg) = breg() else { return };
    let after = arrows().lock().unwrap_or_else(|e| e.into_inner());
    for (eid, (prev, was_stuck)) in before {
        // `landed` covers the plain-arrow stick and the trident path, which
        // dies on block impact and leaves the list entirely.
        let landed = after
            .iter()
            .find(|a| a.eid == eid)
            .map(|a| a.stuck && !was_stuck)
            .unwrap_or(!was_stuck);
        if !landed {
            continue;
        }
        let at = after
            .iter()
            .find(|a| a.eid == eid)
            .map(|a| [a.x, a.y, a.z])
            .unwrap_or([prev[0] + prev[3], prev[1] + prev[4], prev[2] + prev[5]]);
        let cell = at.map(|coord| coord.floor() as i32);
        if reg.name_of(block_at(cell[0], cell[1], cell[2])) == "minecraft:target" {
            w1_live_target_projectile_hit(
                DimId::Overworld,
                RsPos(cell[0], cell[1], cell[2]),
                at,
                w1_dominant_axis([prev[3], prev[4], prev[5]]),
                true,
            );
        }
    }
}

// ---------- W1 / P4b: live vehicle physics ----------

/// Per-vehicle physics state the riding tick carries between game ticks.
#[derive(Default, Clone, Copy)]
struct W1VehiclePhys {
    vel: [f64; 3],
    delta_rot: f64,
    boost_elapsed: u32,
    boost_total: u32,
}

fn w1_vehicle_phys() -> &'static Mutex<HashMap<i32, W1VehiclePhys>> {
    static PHYS: OnceLock<Mutex<HashMap<i32, W1VehiclePhys>>> = OnceLock::new();
    PHYS.get_or_init(Mutex::default)
}

/// Detector-rail cells a live minecart currently occupies: the `powered`
/// property is written while a cart sits on the rail and cleared when the
/// last cart leaves, so the cell emits 15 for exactly the occupied window.
fn w1_detector_rails() -> &'static Mutex<HashSet<(DimId, i32, i32, i32)>> {
    static RAILS: OnceLock<Mutex<HashSet<(DimId, i32, i32, i32)>>> = OnceLock::new();
    RAILS.get_or_init(Mutex::default)
}

/// Write `powered` on a detector rail through the normal block-edit path so
/// the change fans out to the live redstone pass like any other edit.
fn w1_detector_power(dim: DimId, x: i32, y: i32, z: i32, on: bool) {
    let Some(reg) = breg() else { return };
    let st = block_at_dim(dim, x, y, z);
    if reg.name_of(st) != "minecraft:detector_rail" {
        return;
    }
    let next = reg.with_prop(st, "powered", if on { "true" } else { "false" });
    if next != st {
        set_block_dim(dim, x, y, z, next);
        note_block_update_dim(dim, x, y, z, next);
    }
}

/// One `game_tick` of P4b vehicle physics for every live vehicle-kind mob:
/// boats float and paddle, minecarts ride their rails (slowdown, powered
/// boost/brake, detector signal), and rideable mobs steer off the driver's
/// `PlayerInput`. Evaluator outputs move the live entity, re-seat its
/// passengers, and broadcast a `0x70` teleport.
fn w1_vehicle_tick() {
    let Some(reg) = breg() else { return };
    let snap: Vec<(Arc<Mob>, VehicleKind)> = mobs()
        .lock()
        .unwrap()
        .iter()
        .filter_map(|mob| vehicle_kind_of(mob.etype).map(|kind| (mob.clone(), kind)))
        .collect();
    {
        let live: HashSet<i32> = snap.iter().map(|(mob, _)| mob.eid).collect();
        w1_vehicle_phys()
            .lock()
            .unwrap()
            .retain(|eid, _| live.contains(eid));
    }
    if snap.is_empty() {
        return;
    }
    let seats: HashMap<i32, (Option<PaddleBoatPacket>, Option<PlayerInputPacket>, Vec<i32>)> = {
        let live = mounts().lock().unwrap_or_else(|e| e.into_inner());
        snap.iter()
            .map(|(mob, _)| {
                (
                    mob.eid,
                    (
                        live.paddling.get(&mob.eid).copied(),
                        live.steering.get(&mob.eid).copied(),
                        live.registry.passengers_of(mob.eid).to_vec(),
                    ),
                )
            })
            .collect()
    };
    let region = region_guard();
    let mut phys = w1_vehicle_phys().lock().unwrap_or_else(|e| e.into_inner());
    let mut occupied_rails: HashSet<(DimId, i32, i32, i32)> = HashSet::new();
    for (mob, kind) in &snap {
        let (x, y, z, yaw, pitch, ..) = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
        if !region.is_hot(x as i32, z as i32) {
            continue;
        }
        let pos = [x, y, z];
        let dim = mob.dim;
        let (paddle, input, passengers) = seats.get(&mob.eid).cloned().unwrap_or_default();
        let driver = passengers.first().and_then(|eid| {
            clients()
                .lock()
                .unwrap()
                .iter()
                .find(|c| c.eid == *eid)
                .cloned()
        });
        let env = live_vehicle_env(dim, mob.etype, pos, *kind);
        let state = phys.entry(mob.eid).or_default();
        if state.boost_total > 0 {
            state.boost_elapsed += 1;
        }
        let mut new_pos;
        let mut new_yaw = yaw;
        match kind {
            VehicleKind::Boat | VehicleKind::ChestBoat => {
                let paddle = paddle.unwrap_or(PaddleBoatPacket {
                    left: false,
                    right: false,
                });
                let boat_input = WmBoatInput {
                    left: paddle.left,
                    right: paddle.right,
                    up: input.is_some_and(|i| i.forward > 0.0),
                    down: input.is_some_and(|i| i.forward < 0.0),
                };
                let (yaw2, rot, vel) = if let Some(driver) = &driver {
                    let steer = f64::from(driver.pos.lock().unwrap_or_else(|e| e.into_inner()).3);
                    wm_boat_control_tick(boat_input, steer, state.delta_rot, state.vel)
                } else {
                    // An empty boat just damps its hull rotation (cov.J runs
                    // only while a passenger rows).
                    (f64::from(yaw), state.delta_rot * 0.9, state.vel)
                };
                new_yaw = yaw2 as f32;
                let status = if env.on_water {
                    WmBoatStatus::InWater
                } else if mob_has_support(mob.etype, pos[0], pos[1], pos[2]) {
                    WmBoatStatus::OnLand
                } else {
                    WmBoatStatus::InAir
                };
                // The fluid probe is binary, so a floating hull rides at its
                // rest depth fraction.
                let lift = wm_boat_lift(status, 0.5);
                let below_name = if status == WmBoatStatus::OnLand {
                    let below = block_at_dim(
                        dim,
                        pos[0].floor() as i32,
                        (pos[1] - 0.5).floor() as i32,
                        pos[2].floor() as i32,
                    );
                    reg.name_of(below)
                        .strip_prefix("minecraft:")
                        .unwrap_or("")
                        .to_string()
                } else {
                    String::new()
                };
                let (vel2, rot2) = wm_boat_float_tick(
                    status,
                    wm_support_friction(&below_name),
                    WM_BOAT_GRAVITY,
                    vel,
                    lift,
                    rot,
                );
                state.vel = vel2;
                state.delta_rot = rot2;
                new_pos = [pos[0] + vel2[0], pos[1] + vel2[1], pos[2] + vel2[2]];
            }
            VehicleKind::Minecart => {
                let has_passenger = !passengers.is_empty();
                let mut vel = wm_minecart_slowdown(has_passenger, env.on_water, state.vel);
                let (bx, by, bz) =
                    (pos[0].floor() as i32, pos[1].floor() as i32, pos[2].floor() as i32);
                let rail = [by, by - 1].into_iter().find_map(|ry| {
                    let st = block_at_dim(dim, bx, ry, bz);
                    matches!(
                        reg.name_of(st),
                        "minecraft:rail"
                            | "minecraft:powered_rail"
                            | "minecraft:detector_rail"
                            | "minecraft:activator_rail"
                    )
                    .then_some((ry, st))
                });
                if let Some((ry, st)) = rail {
                    let name = reg.name_of(st);
                    if name == "minecraft:powered_rail" {
                        if reg.get_prop(st, "powered").as_deref() == Some("true") {
                            let shape = match reg.get_prop(st, "shape").as_deref() {
                                Some("east_west") => WmRailShape::EastWest,
                                Some("north_south") => WmRailShape::NorthSouth,
                                _ => WmRailShape::Other,
                            };
                            let conductor = |dx: i32, dz: i32| {
                                reg.is_redstone_conductor(block_at_dim(
                                    dim,
                                    bx + dx,
                                    ry,
                                    bz + dz,
                                ))
                            };
                            let (neg, pos_conductor) = match shape {
                                WmRailShape::EastWest => (conductor(1, 0), conductor(-1, 0)),
                                WmRailShape::NorthSouth => (conductor(0, 1), conductor(0, -1)),
                                WmRailShape::Other => (false, false),
                            };
                            let (vx, vz) = wm_minecart_powered_boost(
                                vel[0],
                                vel[2],
                                shape,
                                neg,
                                pos_conductor,
                            );
                            vel[0] = vx;
                            vel[2] = vz;
                        } else {
                            let (vx, vz) = wm_minecart_rail_brake(vel[0], vel[2]);
                            vel[0] = vx;
                            vel[2] = vz;
                        }
                    }
                    let (mx, mz) =
                        wm_minecart_rail_move(vel[0], vel[2], has_passenger, env.on_water);
                    vel[0] = mx;
                    vel[2] = mz;
                    // The rail carries y: the cart rides 0.0625 above the
                    // rail block's top face.
                    new_pos = [pos[0] + vel[0], f64::from(ry) + 0.0625, pos[2] + vel[2]];
                    if name == "minecraft:detector_rail"
                        && wm_detector_rail_signal(true) == 15
                    {
                        occupied_rails.insert((dim, bx, ry, bz));
                    }
                } else {
                    vel[1] -= 0.04;
                    new_pos = [pos[0] + vel[0], pos[1] + vel[1], pos[2] + vel[2]];
                }
                state.vel = vel;
            }
            _ => {
                // Rideable mobs only take physics while a driver sits on them;
                // unridden ones keep their own AI.
                let Some(driver) = &driver else { continue };
                let driver_yaw = f64::from(driver.pos.lock().unwrap_or_else(|e| e.into_inner()).3);
                new_yaw = driver_yaw as f32;
                let input = input.unwrap_or(PlayerInputPacket {
                    sideways: 0.0,
                    forward: 0.0,
                    jump: false,
                    unmount: false,
                });
                let boost = if state.boost_total > state.boost_elapsed {
                    wm_boost_multiplier(state.boost_elapsed, state.boost_total)
                } else {
                    1.0
                };
                let speed = match kind {
                    VehicleKind::Pig => wm_pig_ride_speed(env.move_speed, boost),
                    VehicleKind::Strider => {
                        wm_strider_ride_speed(env.move_speed, !env.on_lava, boost)
                    }
                    _ => env.move_speed * boost,
                };
                let fwd = f64::from(input.forward.clamp(-1.0, 1.0));
                let side = f64::from(input.sideways.clamp(-1.0, 1.0));
                let rad = driver_yaw.to_radians();
                let mx = -rad.sin() * fwd - rad.cos() * side;
                let mz = rad.cos() * fwd - rad.sin() * side;
                let len = mx.hypot(mz);
                let (sx, sz) = if len > 0.0 {
                    (mx / len * speed, mz / len * speed)
                } else {
                    (0.0, 0.0)
                };
                let grounded = mob_has_support(mob.etype, pos[0], pos[1], pos[2]);
                state.vel[0] = sx;
                state.vel[2] = sz;
                if input.jump
                    && grounded
                    && matches!(
                        kind,
                        VehicleKind::Horse
                            | VehicleKind::Donkey
                            | VehicleKind::Mule
                            | VehicleKind::Camel
                            | VehicleKind::Llama
                    )
                {
                    // The 0x26 packet is the instant-jump edge; vanilla's
                    // charge meter rides the jump action button instead.
                    let scale = wm_horse_jump_scale(100);
                    state.vel[1] = wm_horse_jump_velocity(
                        0.7,
                        scale,
                        1.0,
                        wm_jump_boost_power(None),
                    );
                    if fwd > 0.0 {
                        let (jx, jz) = wm_horse_jump_horizontal(driver_yaw, scale);
                        state.vel[0] += jx;
                        state.vel[2] += jz;
                    }
                }
                if *kind == VehicleKind::Strider && env.on_lava {
                    state.vel = wm_strider_lava_buoyancy(state.vel);
                } else if !grounded {
                    state.vel[1] -= 0.08;
                } else {
                    state.vel[1] = 0.0;
                }
                new_pos = [
                    pos[0] + state.vel[0],
                    pos[1] + state.vel[1],
                    pos[2] + state.vel[2],
                ];
            }
        }
        // A falling vehicle stops on the terrain surface; the fluid paths
        // (in_water boat float, strider buoyancy) own their own y.
        let on_fluid = env.on_water || (*kind == VehicleKind::Strider && env.on_lava);
        if new_pos[1] < pos[1] && !on_fluid {
            let ground = height_at_dim(dim, new_pos[0].floor() as i32, new_pos[2].floor() as i32);
            if new_pos[1] < f64::from(ground) {
                new_pos[1] = f64::from(ground);
                state.vel[1] = 0.0;
            }
        }
        *mob.pos.lock().unwrap_or_else(|e| e.into_inner()) = (new_pos[0], new_pos[1], new_pos[2], new_yaw, pitch);
        // `Entity.positionRider`: every seated passenger follows the vehicle
        // and is credited for the carry.
        for (seat, &passenger) in passengers.iter().enumerate() {
            let off = kind.seat_offset(seat);
            if let Some(client) = clients()
                .lock()
                .unwrap()
                .iter()
                .find(|c| c.eid == passenger)
                .cloned()
            {
                let mut p = client.pos.lock().unwrap_or_else(|e| e.into_inner());
                let delta = [
                    new_pos[0] + off[0] - p.0,
                    new_pos[1] + off[1] - p.1,
                    new_pos[2] + off[2] - p.2,
                ];
                let (yaw2, pitch2, ground2) = (p.3, p.4, p.5);
                *p = (
                    new_pos[0] + off[0],
                    new_pos[1] + off[1],
                    new_pos[2] + off[2],
                    yaw2,
                    pitch2,
                    ground2,
                );
                drop(p);
                ac_note_carried_motion(passenger, delta);
            }
        }
        mounts()
            .lock()
            .unwrap()
            .registry
            .register_rider(mob.eid, new_pos);
        broadcast_seen(mob.eid, |_| {
            (
                0x70,
                entity_teleport(mob.eid, new_pos[0], new_pos[1], new_pos[2], new_yaw, pitch, true),
            )
        });
    }
    drop(phys);
    // Detector rails emit 15 exactly while a live cart stands on them.
    let mut rails = w1_detector_rails().lock().unwrap_or_else(|e| e.into_inner());
    for key in &occupied_rails {
        if rails.insert(*key) {
            w1_detector_power(key.0, key.1, key.2, key.3, true);
        }
    }
    let vacated: Vec<_> = rails.difference(&occupied_rails).copied().collect();
    for key in vacated {
        rails.remove(&key);
        w1_detector_power(key.0, key.1, key.2, key.3, false);
    }
}

/// One live `mob_tick` step for the four P6a families `MobBehaviorState` does
/// not enumerate. `target`/`perception` are the same snapshot the generic
/// profile consumed this tick.
fn w1_hostile_mob_tick(
    m: &Mob,
    dim: DimId,
    pos: [f64; 3],
    target: Option<&LiveMobPlayer<'_>>,
    perception: Option<AiEntityPerception>,
    players: &[LiveMobPlayer<'_>],
    mobs_snap: &[Arc<Mob>],
) {
    let visible = target.zip(perception).filter(|(_, seen)| seen.line_of_sight);
    let mut states = w1_live_mob_ai().lock().unwrap_or_else(|e| e.into_inner());
    let ai = states
        .entry(m.eid)
        .or_insert_with(|| w1_live_mob_ai_for(m.etype));
    match ai {
        W1LiveMobAi::Phantom(phantom) => {
            let mut rng = world_rng::next_u64();
            let tick = phantom.tick(
                pos,
                visible.map(|(player, _)| (player.client.eid, player.pos.into())),
                &mut |bound| {
                    let draw = (rng % u64::from(bound.max(1))) as u32;
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    draw
                },
            );
            drop(states);
            // The phantom owns its motion: Circle/Swoop/Climb velocities are
            // per-tick displacements, applied straight to the live position.
            if tick.velocity != [0.0; 3] {
                let (x, y, z, yaw, pitch) = {
                    let mut p = m.pos.lock().unwrap_or_else(|e| e.into_inner());
                    p.0 += tick.velocity[0];
                    p.1 += tick.velocity[1];
                    p.2 += tick.velocity[2];
                    *p
                };
                broadcast_seen(m.eid, |_| {
                    (0x70, entity_teleport(m.eid, x, y, z, yaw, pitch, false))
                });
            }
            if let Some(victim) = tick.attack {
                if let Some(player) = players
                    .iter()
                    .find(|p| p.dim == dim && p.client.eid == victim)
                {
                    live_mob_melee(m, player.client, pos, PHANTOM_ATTACK_DAMAGE, 0.35);
                }
            }
        }
        W1LiveMobAi::Pillager(pillager) => {
            let action = pillager.tick(visible.map(|(player, _)| player.client.eid));
            drop(states);
            if let Some(PillagerAction::Shoot { .. }) = action {
                if let Some(player) = target {
                    live_skeleton_shoot(m, pos, player.pos);
                }
            }
        }
        W1LiveMobAi::Evoker(evoker) => {
            let sheep: Vec<(i32, [f64; 3])> = mobs_snap
                .iter()
                .filter(|sheep| sheep.etype == 87 && sheep.dim == dim)
                .filter(|sheep| {
                    w1_wool_colors()
                        .lock()
                        .unwrap()
                        .get(&sheep.eid)
                        .map_or(true, |&color| color == 11)
                })
                .map(|sheep| {
                    let p = *sheep.pos.lock().unwrap_or_else(|e| e.into_inner());
                    (sheep.eid, [p.0, p.1, p.2])
                })
                .collect();
            let event = evoker.tick(
                pos,
                visible.map(|(player, _)| (player.client.eid, player.pos.into())),
                &sheep,
            );
            drop(states);
            match event {
                Some(EvokerEvent::FangAttack { positions }) => {
                    for &[fx, fy, fz] in &positions {
                        let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                        broadcast_seen(m.eid, |_| {
                            (
                                0x01,
                                spawn_entity_pkt(
                                    eid,
                                    36,
                                    f64::from(fx) + 0.5,
                                    f64::from(fy),
                                    f64::from(fz) + 0.5,
                                    0.0,
                                    0.0,
                                    0,
                                ),
                            )
                        });
                        w1_evoker_fangs().lock().unwrap_or_else(|e| e.into_inner()).push((eid, 22));
                    }
                    for player in players.iter().filter(|p| p.dim == dim) {
                        let feet = (
                            player.pos.0.floor() as i32,
                            player.pos.1.floor() as i32,
                            player.pos.2.floor() as i32,
                        );
                        if positions.iter().any(|&[fx, fy, fz]| {
                            fx == feet.0 && fz == feet.2 && (feet.1 - fy).abs() <= 1
                        }) {
                            live_mob_damage_player(
                                player.client,
                                pos,
                                diff_mob_dmg(6.0),
                                Some(0),
                                Some(0.4),
                            );
                        }
                    }
                }
                Some(EvokerEvent::SummonVex { count }) => {
                    let vex_hp = entity_spec_by_id(112).map_or(14.0, |spec| spec.max_health);
                    for i in 0..count {
                        let angle =
                            f64::from(i) * 2.0 * std::f64::consts::PI / f64::from(count.max(1));
                        insert_live_mob(
                            112,
                            dim,
                            (pos[0] + angle.cos(), pos[1] + 1.5, pos[2] + angle.sin(), 0.0, 0.0),
                            vex_hp,
                            None,
                        );
                    }
                }
                Some(EvokerEvent::Wololo { target: sheep_eid }) => {
                    w1_wool_colors().lock().unwrap_or_else(|e| e.into_inner()).insert(sheep_eid, 14);
                    // Sheep color is metadata index 16 (byte); dye 14 = red.
                    let mut meta = Vec::new();
                    write_varint(&mut meta, sheep_eid as i64);
                    meta.push(16);
                    write_varint(&mut meta, 0);
                    meta.push(14);
                    meta.push(0xff);
                    broadcast_seen(m.eid, |_| (0x58, meta.clone()));
                }
                None => {}
            }
        }
        W1LiveMobAi::Guardian(guardian) => {
            let events = guardian.tick(visible.map(|(player, _)| {
                let dx = pos[0] - player.pos.0;
                let dy = pos[1] - player.pos.1;
                let dz = pos[2] - player.pos.2;
                (player.client.eid, (dx * dx + dy * dy + dz * dz).sqrt())
            }));
            drop(states);
            for event in events {
                let (victim, damage) = match event {
                    GuardianEvent::Laser { target, damage } => (target, Some(damage)),
                    GuardianEvent::MiningFatigue { target } => (target, None),
                };
                let Some(player) = players
                    .iter()
                    .find(|p| p.dim == dim && p.client.eid == victim)
                else {
                    continue;
                };
                match damage {
                    Some(damage) => {
                        live_mob_damage_player(
                            player.client,
                            pos,
                            diff_mob_dmg(damage),
                            Some(0),
                            Some(0.25),
                        );
                    }
                    None => {
                        let pkt = entity_effect_pkt(
                            victim,
                            name_to_effect("mining_fatigue").unwrap_or(3),
                            i32::from(ELDER_FATIGUE_AMPLIFIER),
                            ELDER_FATIGUE_DURATION,
                        );
                        if let Ok(mut sock) = player.client.sock.lock() {
                            let _ = send_packet(&mut sock, 0x76, pkt);
                        }
                    }
                }
            }
        }
    }
}

fn mob_tick() {
    let clients = client_snapshot();
    let players: Vec<_> = clients.iter().map(LiveMobPlayer::new).collect();
    // PR #66: despawn before taking the simulation snapshot.
    tick_natural_hostile_despawn();
    let mobs_snap = {
        let mobs = mobs().lock().unwrap();
        let states = &mut *live_mob_states().lock().unwrap();
        let alive: HashSet<_> = mobs.iter().map(|mob| mob.eid).collect();
        LIVE_MOB_AI.with(|ai| ai.borrow_mut().retain(|eid, _| alive.contains(eid)));
        states.retain(|eid, _| alive.contains(eid));
        w1_live_mob_ai()
            .lock()
            .unwrap()
            .retain(|eid, _| alive.contains(eid));
        w1_wool_colors()
            .lock()
            .unwrap()
            .retain(|eid, _| alive.contains(eid));
        mob_custom_names()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|eid, _| alive.contains(eid));
        mobs.clone()
    };
    w1_evoker_fangs_tick();
    if players.is_empty() {
        return;
    }
    let Some(reg) = crate::block_registry::breg() else {
        return;
    };
    tick_live_mob_projectiles(reg, &players);
    for dim in DimId::all_registered() {
        let mobs: Vec<_> = mobs_snap
            .iter()
            .filter(|mob| mob.dim == dim)
            .cloned()
            .collect();
        if !mobs.is_empty() && players.iter().any(|player| player.dim == dim) {
            live_mob_tick_dimension(dim, reg, &mobs, &players);
        }
    }
}

fn live_mob_tick_dimension(
    dim: DimId,
    reg: &crate::block_registry::BReg,
    mobs_snap: &[Arc<Mob>],
    players: &[LiveMobPlayer<'_>],
) {
    let clients: Vec<_> = players
        .iter()
        .filter(|p| p.dim == dim)
        .map(|p| p.client.clone())
        .collect();
    let grid = EntitySpatialGrid::build_mobs_and_clients(mobs_snap, &clients);
    let (dead_by_cramming, hot) = {
        let region = region_guard();
        // The existing push and fluid owners use Overworld block lookups.
        let dead = if dim == DimId::Overworld {
            apply_entity_push_step(mobs_snap, &clients, &region, &grid)
        } else {
            vec![false; mobs_snap.len()]
        };
        let hot: Vec<_> = grid
            .entities
            .iter()
            .take(mobs_snap.len())
            .map(|entity| region.is_hot(entity.x.floor() as i32, entity.z.floor() as i32))
            .collect();
        (dead, hot)
    };
    let mut breed_scratch = SpatialScratch::default();
    let mut breed_cands = Vec::new();
    let world = LiveWorldPathAdapter::new(dim, reg);
    let crystals: Vec<_> = mobs_snap
        .iter()
        .filter(|mob| mob.etype == 30)
        .map(|mob| {
            let (x, y, z, ..) = *mob.pos.lock().unwrap();
            DragonCrystal {
                entity: mob.eid,
                position: [x, y, z],
            }
        })
        .collect();
    let tick = TICK.load(Ordering::Relaxed);
    let wt = WORLD_TIME.load(Ordering::Relaxed) % 24000;
    let day = !(13000..=23000).contains(&wt);
    let raining = dim == DimId::Overworld && RAINING.load(Ordering::Relaxed) != 0;
    let hostile = DIFFICULTY.load(Ordering::Relaxed) != 0;
    // Under real overload the smoother shrinks how far from a player a mob
    // still gets its AI tick; at the baseline range this check never runs.
    let activation = current_load_budget().entity_activation_range;
    let activation_sq = (activation as f64) * (activation as f64);
    for (mob_idx, m) in mobs_snap.iter().enumerate() {
        if dead_by_cramming[mob_idx] {
            continue;
        }
        let mut mp = m.pos.lock().unwrap();
        if *m.hp.lock().unwrap() <= 0.0 {
            let (mx, my, mz, ..) = *mp;
            drop(mp);
            mob_env_die(m.eid, m.etype, mx, my, mz, *m.fire_ticks.lock().unwrap() > 0);
            continue;
        }
        if !hot[mob_idx] {
            continue;
        }
        if activation < LoadBudget::FULL.entity_activation_range
            && !players.iter().any(|p| {
                p.dim == dim
                    && {
                        let dx = p.pos.0 - mp.0;
                        let dy = p.pos.1 - mp.1;
                        let dz = p.pos.2 - mp.2;
                        dx * dx + dy * dy + dz * dz <= activation_sq
                    }
            })
        {
            continue;
        }
        let Some(spec) = entity_spec_by_id(m.etype) else {
            continue;
        };
        if spec.max_health <= 0.0 {
            continue;
        }
        // Vanilla `TamableAnimal` follow: a standing tamed wolf/cat more than
        // 12 blocks behind its owner teleports onto a safe floor next to them
        // rather than being left behind. The owner must be online and in the
        // pet's dimension; a sitting pet never moves. Mirrors the
        // behavior-effect teleport below — a mob that teleported is done for
        // the tick.
        if matches!(m.etype, 15 | 122) {
            let pet_state = live_mob_states()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&m.eid)
                .and_then(|state| match &state.behavior {
                    Some(MobBehaviorState::Wolf(pet) | MobBehaviorState::Cat(pet)) => {
                        Some((pet.owner, pet.sitting))
                    }
                    _ => None,
                });
            if let Some((Some(owner), sitting)) = pet_state {
                if let Some(owner_pos) = players
                    .iter()
                    .find_map(|p| (p.dim == dim && p.client.uuid == owner).then_some(p.pos))
                {
                    if let Some((tx, ty, tz)) = pet_follow_teleport_step(
                        dim,
                        owner_pos,
                        (mp.0, mp.1, mp.2),
                        sitting,
                        |d, x, y, z| reg.blocks_motion(block_at_dim(d, x, y, z)),
                    ) {
                        let (yaw, pitch) = (mp.3, mp.4);
                        *mp = (tx, ty, tz, yaw, pitch);
                        drop(mp);
                        m.vel
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .fill(0.0);
                        broadcast_seen(m.eid, |_| {
                            (0x70, entity_teleport(m.eid, tx, ty, tz, yaw, pitch, true))
                        });
                        continue;
                    }
                }
            }
        }
        let [bx, by, bz] = [mp.0, mp.1, mp.2].map(|value| value.floor() as i32);
        let feet_state = block_at_dim(dim, bx, by, bz);
        let (in_water, in_lava) = if dim == DimId::Overworld {
            (
                mob_in_water(m.etype, mp.0, mp.1, mp.2),
                mob_in_lava(m.etype, mp.0, mp.1, mp.2),
            )
        } else {
            (
                reg.has_water(feet_state),
                reg.fluid_name(feet_state) == "minecraft:lava",
            )
        };
        if in_water {
            *m.fire_ticks.lock().unwrap() = 0;
        }
        // GP-1 hands vertical travel in fluids to LivingEntity.travel.
        if dim == DimId::Overworld && (in_water || in_lava) {
            if let Some(travel) = mob_fluid_travel_tick(m, &mut mp) {
                if travel.moved != [0.0; 3] {
                    let (x, y, z, yaw, pitch) = *mp;
                    let grounded = mob_has_support(m.etype, x, y, z);
                    broadcast_seen(m.eid, |_| {
                        (0x70, entity_teleport(m.eid, x, y, z, yaw, pitch, grounded))
                    });
                }
            }
        } else {
            m.vel.lock().unwrap().fill(0.0);
        }
        let submerged =
            dim == DimId::Overworld && mob_drowning_submerged(m.etype, mp.0, mp.1, mp.2);
        let mut damage = 0.0;
        if tick.is_multiple_of(server_ticks(10)) {
            if m.etype != 31
                && if dim == DimId::Overworld {
                    mob_cell_solid(mp.0, mp.1 + 1.0, mp.2)
                } else {
                    reg.blocks_motion(block_at_dim(dim, bx, by + 1, bz))
                }
            {
                damage += 1.0;
            }
            if in_lava
                && !mob_fire_immune(m.etype)
                && GR_FIRE_DAMAGE.load(Ordering::Relaxed) != 0
            {
                damage += 4.0;
            }
        }
        if !matches!(m.etype, 124 | 91) {
            let mut air = m.air.lock().unwrap();
            if !submerged {
                *air = server_ticks(AIR_SUPPLY_GT) as i32;
            } else if *air > 0 {
                *air -= 1;
            } else if tick.is_multiple_of(server_ticks(20))
                && GR_DROWNING_DAMAGE.load(Ordering::Relaxed) != 0
            {
                damage += 2.0;
            }
        }
        if matches!(m.etype, 124 | 91 | 76)
            && dim == DimId::Overworld
            && day
            && !in_water
            && !raining
            && by + 1 >= height_at(bx, bz)
            && block_at(bx, by + 2, bz) == 0
        {
            let sunlight_burn = server_ticks(160) as i32;
            let mut fire_ticks = m.fire_ticks.lock().unwrap();
            *fire_ticks = (*fire_ticks).max(sunlight_burn);
        }
        {
            let mut fire_ticks = m.fire_ticks.lock().unwrap();
            let (remaining, fire_damage) =
                fire_tick_update(*fire_ticks, tick, mob_fire_immune(m.etype));
            *fire_ticks = remaining;
            if fire_damage && GR_FIRE_DAMAGE.load(Ordering::Relaxed) != 0 {
                damage += 1.0;
            }
        }
        if m.etype == 8 && in_water && tick.is_multiple_of(server_ticks(20)) {
            damage += blaze_hazard_damage(BlazeHazard::Water);
        }
        // Powder snow freeze damage for mobs; the vanilla
        // `freeze_immune_entity_types` tag (polar bear, snow golem, stray,
        // wither) is exempt.
        if tick.is_multiple_of(server_ticks(40))
            && GR_FREEZE_DAMAGE.load(Ordering::Relaxed) != 0
            && !matches!(m.etype, 81 | 96 | 102 | 119)
            && reg.name_of(feet_state) == "minecraft:powder_snow"
        {
            damage += 1.0;
        }
        if damage > 0.0 {
            let hp = {
                let mut hp = m.hp.lock().unwrap();
                *hp -= damage;
                *hp
            };
            *m.hurt_at.lock().unwrap() = Instant::now();
            broadcast_seen(m.eid, |_| (0x24, hurt_anim(m.eid)));
            if hp <= 0.0 {
                let (x, y, z, ..) = *mp;
                drop(mp);
                mob_env_die(
                    m.eid,
                    m.etype,
                    x,
                    y,
                    z,
                    in_lava || *m.fire_ticks.lock().unwrap() > 0,
                );
                continue;
            }
        }

        let position = (mp.0, mp.1, mp.2);
        let pos = [mp.0, mp.1, mp.2];
        drop(mp);
        let distance = |player: &&LiveMobPlayer<'_>| entity_distance_sq(position, player.pos);
        let local = || players.iter().filter(|player| player.dim == dim);
        let nearest = local()
            .filter(|p| !p.client.is_spectator())
            .min_by(|a, b| distance(a).total_cmp(&distance(b)));
        let target = local()
            .filter(|p| p.attackable && hostile && distance(p) <= 64.0f64.powi(2))
            .min_by(|a, b| distance(a).total_cmp(&distance(b)));
        let target = if matches!(
            m.etype,
            8 | 23 | 29 | 31 | 35 | 45 | 50 | 76 | 80 | 91 | 116 | 118 | 119 | 124 | 100 | 16
                | 27 | 33 | 54 | 88 | 102
        ) {
            target
        } else {
            None
        };
        let perception = target.map(|target| {
            let mut p = target.perceive();
            p.line_of_sight = live_mob_block_hit(
                dim,
                reg,
                [pos[0], pos[1] + f64::from(spec.eye_height), pos[2]],
                [p.position.0, p.position.1 + 1.62, p.position.2],
            )
            .is_none();
            p
        });

        if matches!(
            m.etype,
            8 | 15 | 31 | 33 | 45 | 51 | 67 | 78 | 88 | 93 | 116 | 118 | 119 | 122
        ) {
            let hp = *m.hp.lock().unwrap();
            let effects = live_mob_behavior_step(
                m,
                reg,
                MobBehaviorTickCtx {
                    dim,
                    pos,
                    hp,
                    max_hp: spec.max_health,
                    in_water,
                    in_rain: raining && by + 1 >= height_at(bx, bz),
                    on_fire: reg.name_of(feet_state) == "minecraft:fire",
                    nearest_target: target.zip(perception).map(|(target, seen)| {
                        MobTargetSnapshot {
                            eid: seen.entity_id,
                            pos: [target.pos.0, target.pos.1, target.pos.2],
                            hp: *target.client.hp.lock().unwrap(),
                            visible: seen.line_of_sight,
                        }
                    }),
                    rng_u64: world_rng::next_u64(),
                },
                target,
                &crystals,
            );
            let teleported = effects
                .iter()
                .any(|effect| matches!(effect, MobBehaviorEffect::Teleport { .. }));
            if !apply_live_mob_effects(m, dim, pos, effects, players) || teleported {
                continue;
            }
        }

        if w1_hostile_ai_owns(m.etype) {
            w1_hostile_mob_tick(m, dim, pos, target, perception, players, mobs_snap);
            // The phantom's Circle/Swoop/Climb evaluator owns its motion.
            if m.etype == 76 {
                continue;
            }
        }

        if m.etype == 87
            && dim == DimId::Overworld
            && sheared().lock().unwrap().contains(&m.eid)
            && block_at(bx, by - 1, bz) == name_to_state("grass_block").unwrap_or(u32::MAX)
            && ((m.eid as u64).wrapping_mul(1103515245) ^ tick)
                .is_multiple_of(MOB_SPAWN_CHANCE_DENOMINATOR)
        {
            set_block(bx, by - 1, bz, name_to_state("dirt").unwrap_or(0));
            sheared().lock().unwrap().remove(&m.eid);
            note_block_update(bx, by - 1, bz, name_to_state("dirt").unwrap_or(0));
        }

        let in_love = {
            let mut love = m.love.lock().unwrap();
            *love = (*love - 1).max(0);
            *love > 0
        };
        let mate = if in_love {
            grid.query_radius_xz_into(
                pos[0],
                pos[2],
                8.0,
                m.eid,
                &mut breed_scratch,
                &mut breed_cands,
            );
            breed_cands.iter().find_map(|&slot| {
                let entity = &grid.entities[slot as usize];
                if !entity.is_mob || entity.etype != m.etype || dead_by_cramming[entity.idx] {
                    return None;
                }
                let mate = &mobs_snap[entity.idx];
                if *mate.love.lock().unwrap() <= 0 || *mate.hp.lock().unwrap() <= 0.0 {
                    return None;
                }
                let (x, y, z, ..) = *mate.pos.lock().unwrap();
                let mut p = AiEntityPerception::new(mate.eid, (x, y, z));
                p.in_love = true;
                p.compatible = true;
                Some((mate, p))
            })
        } else {
            None
        };
        // Keep immediate breeding for already-nearby animals, including cramming fixtures.
        if let Some((mate, p)) = mate {
            if entity_distance_sq(position, p.position) <= 9.0 {
                live_mob_breed(m, mate, dim, position, p.position);
                continue;
            }
        }

        let tempting = match m.etype {
            19 => Some(853),
            22 | 87 => Some(854),
            77 => Some(1097),
            _ => None,
        };
        let look = tempting
            .and_then(|item| {
                local()
                    .filter(|p| {
                        p.held_item == Some(item) && !p.client.is_spectator() && distance(p) < 100.0
                    })
                    .min_by(|a, b| distance(a).total_cmp(&distance(b)))
            })
            .or(nearest);
        let goal_ctx = GoalContext {
            position,
            width: spec.width,
            height: f64::from(spec.height),
            in_water,
            in_lava,
            on_fire: reg.name_of(feet_state) == "minecraft:fire",
            recently_hurt: m.hurt_at.lock().unwrap().elapsed() < Duration::from_secs(2),
            in_love,
            target: perception
                .filter(|p| entity_distance_sq(position, p.position) <= 24.0f64.powi(2)),
            nearest_player: look.map(LiveMobPlayer::perceive),
            mate: mate.map(|(_, p)| p),
            world: Some(&world),
        };
        let (result, waypoint_y) = LIVE_MOB_AI.with(|ai| {
            let mut ai = ai.borrow_mut();
            let profile = ai.entry(m.eid).or_insert_with(|| new_live_mob_profile(m));
            let result = tick_mob_ai_step(profile, &goal_ctx);
            (
                result,
                profile.navigation.next_waypoint(&world).map(|p| p.1),
            )
        });
        if m.etype == 23 {
            // Water must not clear an already-active fuse.
            let swell = result.swell.unwrap_or(false)
                || (in_water
                    && nearest.is_some_and(|p| {
                        (p.pos.0 - pos[0]).powi(2) + (p.pos.2 - pos[2]).powi(2) < 9.0
                    }));
            let mut fuse = m.fuse.lock().unwrap();
            *fuse = if swell { *fuse + 1 } else { (*fuse - 1).max(0) };
            if *fuse >= server_ticks(CREEPER_IGNITE_GT) as i32 {
                drop(fuse);
                if remove_live_mob(m.eid).is_some() {
                    w1_live_mob_explosion(dim, pos, 3.0, players);
                    broadcast_seen_destroy(m.eid);
                }
                continue;
            }
        }
        if let Some((mate, p)) = mate {
            if result.breed_target == Some(p.entity_id) {
                live_mob_breed(m, mate, dim, position, p.position);
            }
        }
        let sitting = matches!(m.etype, 15 | 122)
            && live_mob_states().lock().unwrap().get(&m.eid).is_some_and(|state| {
                matches!(&state.behavior, Some(MobBehaviorState::Cat(pet) | MobBehaviorState::Wolf(pet)) if pet.sitting)
            });
        live_mob_move(
            m,
            waypoint_y,
            dim,
            reg,
            if sitting {
                MobAiStepResult::default()
            } else {
                result
            },
            in_water || in_lava,
        );
        if let Some(player) = target.filter(|p| result.attack_target == Some(p.client.eid)) {
            if m.etype == 91 && dim == DimId::Overworld {
                live_skeleton_shoot(m, pos, player.pos);
            } else if m.etype != 91
                && !w1_hostile_ai_owns(m.etype)
                && perception.is_some_and(|p| p.line_of_sight)
            {
                live_mob_melee(m, player.client, pos, 3.0, 0.35);
            }
        }
    }
}

fn live_mob_breed(mob: &Mob, mate: &Mob, dim: DimId, a: (f64, f64, f64), b: (f64, f64, f64)) {
    let (x, z) = ((a.0 + b.0) / 2.0, (a.2 + b.2) / 2.0);
    let y = if dim == DimId::Overworld {
        surface_y(x.floor() as i32, z.floor() as i32)
    } else {
        a.1
    };
    let hp = default_mob_hp(entity_spec_by_id(mob.etype).unwrap().name);
    if insert_live_mob(mob.etype, dim, (x, y, z, 0.0, 0.0), hp, None) {
        *mob.love.lock().unwrap() = 0;
        *mate.love.lock().unwrap() = 0;
        xps().lock().unwrap().push(Xp {
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            x,
            y,
            z,
            val: (1 + world_rng::next_u64() % 7) as i16,
            age: 0,
        });
    }
}

// ---------- world ----------
// overlay edits: (dim,cx,cz) -> { packed_local_idx -> blockstate }
static WORLD: Registry<DimChunkKey, HashMap<u32, u32>> = OnceLock::new();
/// Wait/hold split for the two locks the tick thread and the connection thread share.
///
/// The tick profile put 10.8 ms of an 11.4 ms tick body in `environment`, a step that
/// computes a clock read and a 1 Hz broadcast — so the question is what it waits for.
/// `MC_SPINE_LOCKPROFILE=1` answers it for `WORLD` (the block overlay) and `REGION`.
pub(crate) mod lockprof {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub(super) static WORLD_WAIT_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static WORLD_HOLD_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static WORLD_N: AtomicU64 = AtomicU64::new(0);
    pub(super) static REGION_WAIT_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static REGION_HOLD_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static REGION_N: AtomicU64 = AtomicU64::new(0);
    pub(super) fn enabled() -> bool {
        crate::trace_env::env_flag!("MC_SPINE_LOCKPROFILE")
    }
    pub(super) fn report() {
        let ms = |v: &AtomicU64| v.load(Ordering::Relaxed) as f64 / 1e6;
        eprintln!(
            "LOCKPROF world wait={:.0}ms hold={:.0}ms n={} | region wait={:.0}ms hold={:.0}ms n={}",
            ms(&WORLD_WAIT_NS),
            ms(&WORLD_HOLD_NS),
            WORLD_N.load(Ordering::Relaxed),
            ms(&REGION_WAIT_NS),
            ms(&REGION_HOLD_NS),
            REGION_N.load(Ordering::Relaxed)
        );
    }
}

/// A guard that records how long its acquisition waited and how long it was held.
struct TimedGuard<T: 'static> {
    guard: std::sync::MutexGuard<'static, T>,
    waited: Option<u64>,
    held_since: Option<std::time::Instant>,
    which: u8, // 0 = world, 1 = region
}

impl<T: 'static> std::ops::Drop for TimedGuard<T> {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        let (Some(waited), Some(since)) = (self.waited, self.held_since) else {
            return;
        };
        let held = since.elapsed().as_nanos() as u64;
        if self.which == 0 {
            lockprof::WORLD_HOLD_NS.fetch_add(held, Ordering::Relaxed);
            lockprof::WORLD_WAIT_NS.fetch_add(waited, Ordering::Relaxed);
            lockprof::WORLD_N.fetch_add(1, Ordering::Relaxed);
        } else {
            lockprof::REGION_HOLD_NS.fetch_add(held, Ordering::Relaxed);
            lockprof::REGION_WAIT_NS.fetch_add(waited, Ordering::Relaxed);
            lockprof::REGION_N.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl<T: 'static> std::ops::Deref for TimedGuard<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}
impl<T: 'static> std::ops::DerefMut for TimedGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

fn timed_world() -> TimedGuard<HashMap<DimChunkKey, HashMap<u32, u32>>> {
    let prof = lockprof::enabled();
    let t = prof.then(std::time::Instant::now);
    let guard = world().lock().unwrap();
    TimedGuard {
        guard,
        waited: t.map(|t| t.elapsed().as_nanos() as u64),
        held_since: t,
        which: 0,
    }
}

fn timed_region() -> TimedGuard<Region> {
    let prof = lockprof::enabled();
    let t = prof.then(std::time::Instant::now);
    let guard = region().lock().unwrap();
    TimedGuard {
        guard,
        waited: t.map(|t| t.elapsed().as_nanos() as u64),
        held_since: t,
        which: 1,
    }
}

fn world() -> &'static LockedMap<DimChunkKey, HashMap<u32, u32>> {
    WORLD.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The Overworld's edits for one chunk, from a held `WORLD` guard.
///
/// Several derived caches are still keyed by chunk alone — the state band, the section store, the
/// light pass, the indexes `rebuild_derived_indexes` restores — and every one of them describes
/// the Overworld. They ask through this name, so "which dimension does this cache belong to" has
/// one place to answer in S3 instead of one `(DimId::Overworld, ..)` per reader.
fn overworld_edits<'a>(
    w: &'a HashMap<DimChunkKey, HashMap<u32, u32>>,
    cx: i32,
    cz: i32,
) -> Option<&'a HashMap<u32, u32>> {
    w.get(&(DimId::Overworld, cx, cz))
}
static CROPS: Registry<DimBlockPos, u8> = OnceLock::new();
fn crops() -> &'static LockedMap<DimBlockPos, u8> {
    CROPS.get_or_init(|| Mutex::new(HashMap::new()))
}
#[derive(Clone, Debug, PartialEq)]
struct PendingBlockLoot {
    kind: crate::generation_payload::ContainerKind,
    loot_table: String,
    loot_seed: Option<i64>,
}
static PENDING_BLOCK_LOOT: Registry<DimBlockPos, PendingBlockLoot> = OnceLock::new();
fn pending_block_loot() -> &'static LockedMap<DimBlockPos, PendingBlockLoot> {
    PENDING_BLOCK_LOOT.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Chest-minecart spawn packet; the tracker owns visibility, not mob AI.
fn spawn_generated_cart(cart: &generated_content::Cart) -> Vec<u8> {
    let [x, y, z] = cart.pos().expect("validated generated cart");
    let m = cart.motion();
    let mut out = Vec::new();
    write_varint(&mut out, cart.eid as i64);
    out.extend_from_slice(&cart.uuid().expect("validated cart UUID"));
    write_varint(&mut out, name_to_etype("chest_minecart").unwrap());
    for v in [x, y, z] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    spawn_angles(&mut out, 0., 0.);
    write_varint(&mut out, 0);
    for v in m {
        let q = (v.clamp(-3.9, 3.9) * 8000.0) as i16;
        out.extend_from_slice(&q.to_be_bytes());
    }
    out
}
/// `HangingEntity.setDirection`'s yaw: `Direction.get2DDataValue() * 90` (`south` 0,
/// `west` 90, `north` 180, `east` 270). The facing itself travels as the spawn
/// packet's object data; the angle keeps a client that reads the entity's rotation
/// from agreeing with it.
fn frame_yaw(facing: u8) -> f32 {
    match facing {
        3 => 0.0,
        4 => 90.0,
        2 => 180.0,
        5 => 270.0,
        _ => 0.0,
    }
}
/// `spawn_entity` for a generated hanging entity. The object data is the facing's
/// `Direction.get3DDataValue()` (`ItemFrame.getAddEntityPacket` passes exactly that
/// to `ClientboundAddEntityPacket`; the client rebuilds the direction from it).
fn spawn_generated_frame(frame: &generated_content::Frame) -> Vec<u8> {
    let [x, y, z] = frame.pos().expect("validated generated frame");
    let facing = frame.facing().expect("validated frame facing");
    let etype = if frame.glow() {
        name_to_etype("glow_item_frame")
    } else {
        name_to_etype("item_frame")
    }
    .expect("item frame entity type");
    spawn_entity_pkt(
        frame.eid,
        u32::try_from(etype).expect("item frame entity type fits"),
        x,
        y,
        z,
        frame_yaw(facing),
        0.0,
        i64::from(facing),
    )
}
/// `set_entity_data` for a frame. Both fields are `SynchedEntityData` entries the
/// `ItemFrame` class defines after `Entity`'s eight, so `ItemFrame`'s item is index 8
/// (`ItemStack`, type 7) and its rotation index 9 (`INT`, type 1); `HangingEntity`
/// defines none (its direction is the spawn packet's object data).
fn frame_meta_pkt(frame: &generated_content::Frame) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, frame.eid as i64);
    if let Some((name, count)) = frame.item() {
        if let Some(item) = name_to_item(&name) {
            p.push(8);
            write_varint(&mut p, 7); // ENTITY_DATA_SERIALIZER id: item stack
            write_varint(&mut p, i64::from(count));
            write_varint(&mut p, item);
            write_varint(&mut p, 0); // added components
            write_varint(&mut p, 0); // removed components
        }
    }
    p.push(9);
    write_varint(&mut p, 1); // ENTITY_DATA_SERIALIZER id: int
    write_varint(&mut p, i64::from(frame.rotation()));
    p.push(0xff);
    p
}
fn generated_spawner_chunk_entities(cx: i32, cz: i32) -> Vec<ChunkBlockEntity> {
    generated_content()
        .lock()
        .unwrap()
        .spawners
        .iter()
        .filter_map(|(&pos, tag)| {
            if pos.0 >> 4 != cx || pos.2 >> 4 != cz {
                return None;
            }
            let mut bytes = crate::nbt::write_named(&crate::nbt::NbtString::text(""), tag)
                .expect("owned spawner NBT");
            bytes.drain(1..3); // anonymous network NBT omits the root name length.
            Some((pos, 9, bytes)) // native BuiltInRegistries.BLOCK_ENTITY_TYPE mob_spawner
        })
        .collect()
}
/// Safe tick/chunk boundary: keep ownership transitions under save -> gen ->
/// runtime, never take SAVE_LOCK from a block read holding a mob/player lock.
fn materialize_pending_generation() {
    with_save_guard(|| {
        let pending = worldgen::with_gen(|g| g.take_staged_payloads()).unwrap_or_default();
        materialize_generation_payloads_inner(pending);
    });
}
fn generated_content() -> &'static Mutex<generated_content::Content> {
    static CONTENT: OnceLock<Mutex<generated_content::Content>> = OnceLock::new();
    CONTENT.get_or_init(|| Mutex::new(generated_content::Content::default()))
}
#[cfg(test)]
fn materialize_generation_payloads(
    staged: Vec<(
        (i32, i32, i32),
        crate::generation_payload::GenerationPayload,
    )>,
) {
    if staged.is_empty() {
        return;
    }
    with_save_guard(|| materialize_generation_payloads_inner(staged));
}
fn materialize_generation_payloads_inner(
    staged: Vec<(
        (i32, i32, i32),
        crate::generation_payload::GenerationPayload,
    )>,
) {
    for (pos, payload) in staged {
        match payload {
            crate::generation_payload::GenerationPayload::LootContainer {
                kind,
                loot_table,
                loot_seed,
            } if !matches!(kind, crate::generation_payload::ContainerKind::Furnace) => {
                // `RandomizableContainerBlockEntity` preserves `LootTable`/`LootTableSeed`
                // on placed chests/barrels/pots/dispensers/hoppers until first non-spectator
                // open, automation access, or destruction.
                pending_block_loot().lock().unwrap().insert(
                    ow_block(pos),
                    PendingBlockLoot {
                        kind,
                        loot_table,
                        loot_seed,
                    },
                );
            }
            crate::generation_payload::GenerationPayload::Container { .. }
            | crate::generation_payload::GenerationPayload::LootContainer { .. } => {
                use crate::generation_payload::ContainerKind;
                let (kind, container_slots): (ContainerKind, Vec<(i16, Stack)>) = match payload {
                    crate::generation_payload::GenerationPayload::Container { kind, slots } => (
                        kind,
                        slots
                            .into_iter()
                            .map(|(slot, item, count)| (slot, (item, count, Vec::new())))
                            .collect(),
                    ),
                    crate::generation_payload::GenerationPayload::LootContainer {
                        kind,
                        loot_table,
                        loot_seed,
                    } => {
                        let rolled_slots =
                            roll_loot_container_slots(kind, &loot_table, loot_seed);
                        (kind, rolled_slots.into_iter().collect())
                    }
                    _ => unreachable!(),
                };
                match kind {
                    ContainerKind::Chest | ContainerKind::BlockContainer(_) => {
                        let mut store = chests().lock().unwrap_or_else(|e| e.into_inner());
                        let entry = store.entry(ContainerKey::ow(pos)).or_default();
                        for (slot, stack) in container_slots {
                            entry.insert(slot, stack);
                        }
                    }
                    ContainerKind::Furnace => {
                        let mut store = furnaces().lock().unwrap_or_else(|e| e.into_inner());
                        let furnace = store.entry(ow_block(pos)).or_insert(Furnace {
                            input: (0, 0, Vec::new()),
                            fuel: (0, 0, Vec::new()),
                            output: (0, 0, Vec::new()),
                            burn_left: 0,
                            progress: 0,
                            dirty: false,
                            xp: 0.0,
                            recipe_cache: CachedLookup::default(),
                            fuel_cache: CachedLookup::default(),
                        });
                        for (slot, stack) in container_slots {
                            match slot {
                                0 => furnace.input = stack,
                                1 => furnace.fuel = stack,
                                2 => furnace.output = stack,
                                _ => {}
                            }
                        }
                        furnace.dirty = true;
                    }
                }
            }
            crate::generation_payload::GenerationPayload::Spawner => {
                // The one-shot flag cannot reconstruct this block after cache
                // eviction. Preserve it in the world, but never undo a user edit.
                let state = name_to_state("spawner").unwrap();
                let actual = *world()
                    .lock()
                    .unwrap()
                    .entry((DimId::Overworld, pos.0 >> 4, pos.2 >> 4))
                    .or_default()
                    .entry(local_idx(pos.0, pos.1, pos.2))
                    .or_insert(state);
                if actual == state {
                    generated_content()
                        .lock()
                        .unwrap()
                        .spawners
                        .entry(pos)
                        .or_insert_with(|| generated_content::spawner_nbt(pos));
                }
            }
            crate::generation_payload::GenerationPayload::SpawnerEntity(entity_id) => {
                let state = name_to_state("spawner").unwrap();
                let actual = *world()
                    .lock()
                    .unwrap()
                    .entry((DimId::Overworld, pos.0 >> 4, pos.2 >> 4))
                    .or_default()
                    .entry(local_idx(pos.0, pos.1, pos.2))
                    .or_insert(state);
                if actual == state {
                    generated_content()
                        .lock()
                        .unwrap()
                        .spawners
                        .entry(pos)
                        .or_insert_with(|| generated_content::spawner_nbt_for(pos, &entity_id));
                }
            }
            crate::generation_payload::GenerationPayload::Entity {
                id,
                pos: entity_pos,
                yaw,
            } => {
                if let Some(etype) = name_to_etype(&id) {
                    let mut ms = mobs().lock().unwrap();
                    if ms.len() < MOB_CAP {
                        let mut uuid = [0u8; 16];
                        assert!(os_fill_random(&mut uuid), "entity UUID entropy");
                        uuid[6] = (uuid[6] & 15) | 64;
                        uuid[8] = (uuid[8] & 63) | 128;
                        ms.push(Arc::new(Mob {
                            dim: DimId::Overworld,
                            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                            uuid,
                            etype,
                            pos: Mutex::new((
                                entity_pos[0],
                                entity_pos[1],
                                entity_pos[2],
                                yaw,
                                0.0,
                            )),
                            hp: Mutex::new(default_mob_hp(&id)),
                            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
                            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
                            last_hurt_tick: Mutex::new(None),
                            vel: Mutex::new([0.0; 3]),
                            fuse: Mutex::new(0),
                            fire_ticks: Mutex::new(0),
                            love: Mutex::new(0),
                            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
                        }));
                    }
                }
            }
            crate::generation_payload::GenerationPayload::ChestMinecart { loot_seed } => {
                let mut uuid = [0; 16];
                assert!(os_fill_random(&mut uuid), "entity UUID entropy");
                uuid[6] = (uuid[6] & 15) | 64;
                uuid[8] = (uuid[8] & 63) | 128;
                let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                generated_content()
                    .lock()
                    .unwrap()
                    .carts
                    .push(generated_content::Cart::new(eid, uuid, pos, loot_seed));
            }
            crate::generation_payload::GenerationPayload::ItemFrame {
                glow,
                facing,
                item,
            } => {
                // A hanging entity is not a mob: `Mob` carries hit points, air and a
                // fuse, none of which a frame has. It goes into the generated-content
                // store, so its UUID, facing and framed stack survive a save like a
                // generated cart's loot does.
                let mut gen = generated_content().lock().unwrap();
                if gen.frames.len() >= FRAME_CAP {
                    continue;
                }
                let mut uuid = [0u8; 16];
                assert!(os_fill_random(&mut uuid), "entity UUID entropy");
                uuid[6] = (uuid[6] & 15) | 64;
                uuid[8] = (uuid[8] & 63) | 128;
                let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
                gen.frames.push(generated_content::Frame::new(
                    eid,
                    uuid,
                    pos,
                    glow,
                    facing,
                    item.as_ref().map(|(id, count)| (id.as_str(), *count)),
                ));
            }
            crate::generation_payload::GenerationPayload::Sign(lines) => {
                // Key `front = 1`: the template's text is the front side.
                signs()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert((DimId::Overworld, pos.0, pos.1, pos.2, 1), lines);
            }
        }
    }
}
fn roll_cart_loot(seed: i64) -> HashMap<i16, Stack> {
    let mut out = HashMap::new();
    for entry in crate::chest_loot::abandoned_mineshaft(seed) {
        let Some(item) = name_to_item(entry.item) else {
            continue;
        };
        let mut comps = Vec::new();
        if let Some((ench_name, lvl)) = entry.enchantment {
            if let Some(eid) = enchant_id(ench_name) {
                comps.push((
                    COMP_STORED_ENCHANTMENTS,
                    enchantments_data(&[(eid, i64::from(lvl))], true),
                ));
            }
        }
        out.insert(entry.slot, (item, entry.count, comps));
    }
    out
}
fn unpack_cart_loot_if_needed(uuid: [u8; 16]) {
    with_save_guard(|| {
        let taken = {
            let mut gen = generated_content().lock().unwrap();
            let Some(cart) = gen.carts.iter_mut().find(|c| c.uuid().ok() == Some(uuid)) else {
                return;
            };
            cart.take_loot().map(|(table, seed)| {
                let effective = if seed == 0 {
                    world_rng::next_u64() as i64
                } else {
                    seed
                };
                (table, effective)
            })
        };
        if let Some((table, seed)) = taken {
            if table == "minecraft:chests/abandoned_mineshaft" {
                let slots = roll_cart_loot(seed);
                chests()
                    .lock()
                    .unwrap()
                    .entry(ContainerKey::Cart(uuid))
                    .or_insert(slots);
            }
        }
    });
}
fn break_generated_cart(target_eid: i32, drop_loot: bool) -> bool {
    let broken = with_save_guard(|| {
        let mut gen = generated_content().lock().unwrap();
        let Some(idx) = gen.carts.iter().position(|c| c.eid == target_eid) else {
            return None;
        };
        let mut cart = gen.carts.remove(idx);
        let pos = cart.pos().unwrap_or([0.0, 0.0, 0.0]);
        let uuid = cart.uuid().ok();
        let loot = cart.take_loot();
        drop(gen);
        let mut spilled = Vec::new();
        if let Some(uuid) = uuid {
            if let Some(slots) = chests().lock().unwrap().remove(&ContainerKey::Cart(uuid)) {
                spilled.extend(
                    slots
                        .into_values()
                        .filter(|(it, cnt, _)| *it > 0 && *cnt > 0),
                );
            } else if drop_loot {
                if let Some((table, seed)) = loot {
                    if table == "minecraft:chests/abandoned_mineshaft" {
                        let effective = if seed == 0 {
                            world_rng::next_u64() as i64
                        } else {
                            seed
                        };
                        spilled.extend(
                            roll_cart_loot(effective)
                                .into_values()
                                .filter(|(it, cnt, _)| *it > 0 && *cnt > 0),
                        );
                    }
                }
            }
        }
        Some((uuid, pos, spilled))
    });
    let Some((uuid, pos, spilled)) = broken else {
        return false;
    };
    if drop_loot && gr_do_entity_drops() {
        let mut ds = drops().lock().unwrap();
        if let Some(cart_item) = name_to_item("chest_minecart") {
            ds.push(Drop {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                x: pos[0],
                y: pos[1] + 0.1,
                z: pos[2],
                item: cart_item,
                cnt: 1,
                comps: Vec::new(),
                age: 0,
                target: None,
                thrower: None,
            });
        }
        for (item, cnt, comps) in spilled {
            ds.push(Drop {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                x: pos[0],
                y: pos[1] + 0.1,
                z: pos[2],
                item,
                cnt,
                comps,
                age: 0,
                target: None,
                thrower: None,
            });
        }
    }
    if let Some(uuid) = uuid {
        let key = ContainerKey::Cart(uuid);
        for c in client_snapshot() {
            let close = {
                let mut open = c.open_chest.lock().unwrap();
                if open.is_some_and(|(a, b)| a == key || b == Some(key)) {
                    *open = None;
                    true
                } else {
                    false
                }
            };
            if close {
                *c.chest_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
                if let Ok(mut w) = c.sock.lock() {
                    let _ = send_packet(&mut w, 0x12, vec![1u8]);
                }
            }
        }
    }
    broadcast_seen_destroy(target_eid);
    true
}
fn aabb_overlaps(a: ([f64; 3], [f64; 3]), b: ([f64; 3], [f64; 3])) -> bool {
    a.0[0] < b.1[0]
        && a.1[0] > b.0[0]
        && a.0[1] < b.1[1]
        && a.1[1] > b.0[1]
        && a.0[2] < b.1[2]
        && a.1[2] > b.0[2]
}
fn aabb_collides_blocks(min: [f64; 3], max: [f64; 3]) -> bool {
    collect_block_collision_boxes(min, max)
        .into_iter()
        .any(|(wmin, wmax)| {
            min[0] < wmax[0] - 1e-7
                && max[0] > wmin[0] + 1e-7
                && min[1] < wmax[1] - 1e-7
                && max[1] > wmin[1] + 1e-7
                && min[2] < wmax[2] - 1e-7
                && max[2] > wmin[2] + 1e-7
        })
}
fn cart_comparator_signal(slots: &HashMap<i16, Stack>) -> i32 {
    let mut sum = 0.0f32;
    let mut any = false;
    for slot in 0..27i16 {
        if let Some((item, count, comps)) = slots.get(&slot) {
            if *item > 0 && *count > 0 {
                any = true;
                sum += *count as f32 / item_stack_max(*item, comps).max(1) as f32;
            }
        }
    }
    if any {
        (sum / 27.0 * 14.0).floor() as i32 + 1
    } else {
        0
    }
}
fn cart_in_water(pos: [f64; 3]) -> bool {
    aabb_in_water(EntityAabb::from_dimensions(pos[0], pos[1], pos[2], 0.98, 0.7))
}
struct CartRuntimeWorld;
impl crate::minecart::World for CartRuntimeWorld {
    fn rail(&mut self, at: BlockPos) -> Option<crate::minecart::Rail> {
        let reg = crate::block_registry::breg()?;
        let st = block_at(at.0, at.1, at.2);
        let powered = match reg.name_of(st) {
            "minecraft:rail" | "minecraft:detector_rail" | "minecraft:activator_rail" => None,
            "minecraft:powered_rail" => {
                Some(reg.get_prop(st, "powered").as_deref() == Some("true"))
            }
            _ => return None,
        };
        let shape = match reg.get_prop(st, "shape")?.as_str() {
            "north_south" => crate::minecart::Shape::NorthSouth,
            "east_west" => crate::minecart::Shape::EastWest,
            "ascending_east" => crate::minecart::Shape::AscendingEast,
            "ascending_west" => crate::minecart::Shape::AscendingWest,
            "ascending_north" => crate::minecart::Shape::AscendingNorth,
            "ascending_south" => crate::minecart::Shape::AscendingSouth,
            "south_east" => crate::minecart::Shape::SouthEast,
            "south_west" => crate::minecart::Shape::SouthWest,
            "north_west" => crate::minecart::Shape::NorthWest,
            "north_east" => crate::minecart::Shape::NorthEast,
            _ => return None,
        };
        Some(crate::minecart::Rail { shape, powered })
    }
    fn move_cart(&mut self, pos: [f64; 3], delta: [f64; 3]) -> ([f64; 3], bool) {
        let aabb = EntityAabb::from_dimensions(pos[0], pos[1], pos[2], 0.98, 0.7);
        let res = entity_collide_world(aabb, delta, false, 0.0, &[]);
        (res.delta, res.vertical_collision_below)
    }
    fn conductor(&mut self, at: BlockPos) -> bool {
        let st = block_at(at.0, at.1, at.2);
        crate::block_registry::breg().is_some_and(|reg| reg.is_solid_render(st))
    }
}
fn cart_tick() {
    let carts_snap: Vec<([u8; 16], i32, crate::minecart::State, bool)> = {
        let region = region_guard();
        let gen = generated_content().lock().unwrap();
        gen.carts
            .iter()
            .filter_map(|c| {
                let uuid = c.uuid().ok()?;
                let pos = c.pos().ok()?;
                if !region.is_hot(pos[0].floor() as i32, pos[2].floor() as i32) {
                    return None;
                }
                let state = crate::minecart::State {
                    pos,
                    velocity: c.motion(),
                    on_ground: c.on_ground(),
                    in_water: false,
                };
                Some((uuid, c.eid, state, c.has_loot_table()))
            })
            .collect()
    };
    if carts_snap.is_empty() {
        return;
    }
    let signals: HashMap<[u8; 16], i32> = {
        let ch = chests().lock().unwrap();
        carts_snap
            .iter()
            .filter(|(_, _, _, loot_pending)| !*loot_pending)
            .map(|(uuid, _, _, _)| {
                let sig = ch
                    .get(&ContainerKey::Cart(*uuid))
                    .map(cart_comparator_signal)
                    .unwrap_or(0);
                (*uuid, sig)
            })
            .collect()
    };
    let mut updates = Vec::new();
    let mut world_view = CartRuntimeWorld;
    for (uuid, eid, mut state, loot_pending) in carts_snap {
        let old_pos = state.pos;
        let old_vel = state.velocity;
        let old_ground = state.on_ground;
        state.in_water = cart_in_water(state.pos);
        let signal = signals.get(&uuid).copied().unwrap_or(0);
        state.tick(&mut world_view, loot_pending, signal);
        let pos_changed = state.pos != old_pos || state.on_ground != old_ground;
        let vel_changed = state.velocity != old_vel;
        if pos_changed || vel_changed {
            updates.push((uuid, eid, state, pos_changed, vel_changed));
        }
    }
    if updates.is_empty() {
        return;
    }
    let applied = with_save_guard(|| {
        let mut gen = generated_content().lock().unwrap();
        let mut out = Vec::with_capacity(updates.len());
        for (uuid, eid, state, send_pos, send_vel) in updates {
            if let Some(cart) = gen.carts.iter_mut().find(|c| c.uuid().ok() == Some(uuid)) {
                cart.set_pos(state.pos);
                cart.set_motion(state.velocity);
                cart.set_on_ground(state.on_ground);
                out.push((eid, state, send_pos, send_vel));
            }
        }
        out
    });
    for (eid, state, send_pos, send_vel) in applied {
        if send_pos {
            broadcast_seen(eid, |_c| {
                (
                    0x70,
                    entity_teleport(
                        eid,
                        state.pos[0],
                        state.pos[1],
                        state.pos[2],
                        0.0,
                        0.0,
                        state.on_ground,
                    ),
                )
            });
        }
        if send_vel {
            let [vx, vy, vz] = state.velocity.map(|v| (v.clamp(-3.9, 3.9) * 8000.0) as i16);
            broadcast_seen(eid, |_c| (0x5a, velocity_pkt(eid, vx, vy, vz)));
        }
    }
}
fn current_sky_darken() -> i32 {
    let day_time = WORLD_TIME.load(Ordering::Relaxed) as f64;
    let d = (day_time / 24000.0 - 0.25).rem_euclid(1.0);
    let e = 0.5 - (d * std::f64::consts::PI).cos() / 2.0;
    let sun_angle = (((d * 2.0 + e) / 3.0) as f32) * std::f32::consts::TAU;
    let rain = if RAINING.load(Ordering::Relaxed) > 0 {
        1.0
    } else {
        0.0
    };
    let thunder = if THUNDERING.load(Ordering::Relaxed) > 0 {
        1.0
    } else {
        0.0
    };
    let rd = 1.0 - (rain * 5.0) / 16.0;
    let re = 1.0 - (thunder * 5.0) / 16.0;
    let rf = 0.5 + 2.0 * f64::from(sun_angle.cos()).clamp(-0.25, 0.25);
    ((1.0 - rf * rd * re) * 11.0) as i32
}
fn nibble_at(sections: &[[u8; 2048]], x: i32, y: i32, z: i32) -> i32 {
    nibble_at_dim(sections, DimId::Overworld, x, y, z)
}

fn nibble_at_dim(sections: &[[u8; 2048]], dim: DimId, x: i32, y: i32, z: i32) -> i32 {
    let Some(relative_y) = y
        .checked_sub(dim.light_min_y())
        .and_then(|y| usize::try_from(y).ok())
    else {
        return 0;
    };
    let Some(section) = sections.get(relative_y / 16) else {
        return 0;
    };
    let lx = (x & 15) as usize;
    let lz = (z & 15) as usize;
    i32::from((section[(relative_y & 15) * 128 + lz * 8 + lx / 2] >> ((lx & 1) * 4)) & 15)
}

struct SpawnerRuntimeWorld<'a> {
    clients_snap: &'a [Arc<Client>],
    players: Vec<[f64; 3]>,
    gen_sources: Option<GenSources>,
    light_cache: HashMap<(i32, i32), (Vec<[u8; 2048]>, Vec<[u8; 2048]>)>,
    sky_darken: i32,
    effect_origins: Vec<BlockPos>,
}
impl SpawnerRuntimeWorld<'_> {
    fn spider_aabb(pos: [f64; 3]) -> ([f64; 3], [f64; 3]) {
        (
            [pos[0] - 0.35, pos[1], pos[2] - 0.35],
            [pos[0] + 0.35, pos[1] + 0.5, pos[2] + 0.35],
        )
    }
}
impl crate::spawner::World for SpawnerRuntimeWorld<'_> {
    fn active(&mut self, origin: BlockPos, range: i32) -> bool {
        if range < 0 {
            return false;
        }
        let r = f64::from(range);
        let r2 = r * r;
        let ox = origin.0 as f64 + 0.5;
        let oy = origin.1 as f64 + 0.5;
        let oz = origin.2 as f64 + 0.5;
        self.players.iter().any(|p| {
            let (dx, dy, dz) = (p[0] - ox, p[1] - oy, p[2] - oz);
            dx * dx + dy * dy + dz * dz <= r2
        })
    }
    fn no_collision(&mut self, pos: [f64; 3]) -> bool {
        let (min, max) = Self::spider_aabb(pos);
        !aabb_collides_blocks(min, max)
    }
    fn rules(
        &mut self,
        pos: [f64; 3],
        custom: Option<crate::spawner::LightRules>,
        random: &mut crate::generation_math::Lcg,
    ) -> bool {
        if custom.is_none() && DIFFICULTY.load(Ordering::Relaxed) == 0 {
            return false;
        }
        let bx = pos[0].floor() as i32;
        let by = pos[1].floor() as i32;
        let bz = pos[2].floor() as i32;
        let cx = bx.div_euclid(16);
        let cz = bz.div_euclid(16);
        let gen = self.gen_sources.get_or_insert_with(|| {
            let mut merged: GenSources = HashMap::new();
            for c in self.clients_snap {
                let g = c.gen_sources.lock().unwrap();
                if *c.dim.lock().unwrap() != DimId::Overworld {
                    continue;
                }
                for (&k, v) in g.iter() {
                    merged.entry(k).or_insert_with(|| v.clone());
                }
            }
            merged
        });
        let (bl_secs, sl_secs) = self.light_cache.entry((cx, cz)).or_insert_with(|| {
            (
                block_light_sections(cx, cz, gen),
                sky_light_sections_world(cx, cz),
            )
        });
        let bl = nibble_at(bl_secs, bx, by, bz);
        let sl = nibble_at(sl_secs, bx, by, bz);
        if let Some(c) = custom {
            return (c.block[0]..=c.block[1]).contains(&bl)
                && (c.sky[0]..=c.sky[1]).contains(&sl);
        }
        if sl > random.next_bound(32) {
            return false;
        }
        if bl > 0 {
            return false;
        }
        let darken = if THUNDERING.load(Ordering::Relaxed) > 0 {
            10
        } else {
            self.sky_darken
        };
        bl.max(sl - darken) <= random.next_bound(8)
    }
    fn nearby(&mut self, origin: BlockPos, range: i32) -> usize {
        let cave_spider = name_to_etype("cave_spider").unwrap();
        let box_min = [
            (origin.0 - range) as f64,
            (origin.1 - range) as f64,
            (origin.2 - range) as f64,
        ];
        let box_max = [
            (origin.0 + 1 + range) as f64,
            (origin.1 + 1 + range) as f64,
            (origin.2 + 1 + range) as f64,
        ];
        mobs()
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.dim == DimId::Overworld && m.etype == cave_spider && *m.hp.lock().unwrap() > 0.0)
            .filter(|m| {
                let (mx, my, mz, ..) = *m.pos.lock().unwrap();
                aabb_overlaps(Self::spider_aabb([mx, my, mz]), (box_min, box_max))
            })
            .count()
    }
    fn unobstructed(&mut self, pos: [f64; 3]) -> bool {
        let bb = Self::spider_aabb(pos);
        let cave_spider = name_to_etype("cave_spider").unwrap();
        if self.players.iter().any(|p| {
            aabb_overlaps(
                bb,
                ([p[0] - 0.3, p[1], p[2] - 0.3], [p[0] + 0.3, p[1] + 1.8, p[2] + 0.3]),
            )
        }) {
            return false;
        }
        if mobs()
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.dim == DimId::Overworld && *m.hp.lock().unwrap() > 0.0)
            .any(|m| {
                let (mx, my, mz, ..) = *m.pos.lock().unwrap();
                let mb = if m.etype == cave_spider {
                    Self::spider_aabb([mx, my, mz])
                } else {
                    ([mx - 0.45, my, mz - 0.45], [mx + 0.45, my + 1.8, mz + 0.45])
                };
                aabb_overlaps(bb, mb)
            })
        {
            return false;
        }
        !generated_content()
            .lock()
            .unwrap()
            .carts
            .iter()
            .filter_map(|c| c.pos().ok())
            .any(|cp| {
                aabb_overlaps(
                    bb,
                    (
                        [cp[0] - 0.49, cp[1], cp[2] - 0.49],
                        [cp[0] + 0.49, cp[1] + 0.7, cp[2] + 0.49],
                    ),
                )
            })
    }
    fn insert(&mut self, pos: [f64; 3], yaw: f32) -> bool {
        let mut uuid = [0; 16];
        assert!(os_fill_random(&mut uuid), "entity UUID entropy");
        uuid[6] = (uuid[6] & 15) | 64;
        uuid[8] = (uuid[8] & 63) | 128;
        let cave_spider = name_to_etype("cave_spider").unwrap();
        let mut ms = mobs().lock().unwrap();
        if ms.len() >= MOB_CAP {
            return false;
        }
        ms.push(Arc::new(Mob {
            dim: DimId::Overworld,
            eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
            uuid,
            etype: cave_spider,
            pos: Mutex::new((pos[0], pos[1], pos[2], yaw, 0.0)),
            hp: Mutex::new(12.0),
            last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
            hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
            last_hurt_tick: Mutex::new(None),
            vel: Mutex::new([0.0; 3]),
            fuse: Mutex::new(0),
            fire_ticks: Mutex::new(0),
            love: Mutex::new(0),
            air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
        }));
        true
    }
    fn effects(&mut self, origin: BlockPos, _pos: [f64; 3]) {
        self.effect_origins.push(origin);
    }
}
fn spawner_tick() {
    let mut spawners_snap: Vec<(BlockPos, crate::nbt::Tag)> = {
        let region = region_guard();
        let gen = generated_content().lock().unwrap();
        gen.spawners
            .iter()
            .filter(|(pos, _)| region.is_hot(pos.0, pos.2))
            .map(|(pos, tag)| (*pos, tag.clone()))
            .collect()
    };
    if spawners_snap.is_empty() {
        return;
    }
    let clients_snap = client_snapshot();
    let players: Vec<[f64; 3]> = clients_snap
        .iter()
        .filter(|c| !c.is_spectator() && *c.dim.lock().unwrap() == DimId::Overworld)
        .map(|c| {
            let p = *c.pos.lock().unwrap();
            [p.0, p.1, p.2]
        })
        .collect();
    let mut world_view = SpawnerRuntimeWorld {
        clients_snap: &clients_snap,
        players,
        gen_sources: None,
        light_cache: HashMap::new(),
        sky_darken: current_sky_darken(),
        effect_origins: Vec::new(),
    };
    let mut rng = crate::generation_math::Lcg::new(crate::world_rng::next_u64());
    for (origin, tag) in &mut spawners_snap {
        let _ = crate::spawner::tick(tag, *origin, &mut world_view, &mut rng);
    }
    with_save_guard(|| {
        let mut gen = generated_content().lock().unwrap();
        for (origin, tag) in spawners_snap {
            if let Some(slot) = gen.spawners.get_mut(&origin) {
                *slot = tag;
            }
        }
    });
    for origin in world_view.effect_origins {
        let mut p = Vec::new();
        p.extend_from_slice(&2004i32.to_be_bytes());
        write_pos(&mut p, origin.0, origin.1, origin.2);
        p.extend_from_slice(&0i32.to_be_bytes());
        p.push(0);
        let ox = origin.0 as f64 + 0.5;
        let oy = origin.1 as f64 + 0.5;
        let oz = origin.2 as f64 + 0.5;
        for c in &clients_snap {
            if *c.dim.lock().unwrap() != DimId::Overworld {
                continue;
            }
            let (px, py, pz, ..) = *c.pos.lock().unwrap();
            let (dx, dy, dz) = (px - ox, py - oy, pz - oz);
            if dx * dx + dy * dy + dz * dz <= 64.0 * 64.0 {
                if let Ok(mut w) = c.sock.lock() {
                    let _ = send_packet(&mut w, 0x28, p.clone());
                }
            }
        }
    }
}
const MIN_Y: i32 = -64;

// ---- procedural terrain (deterministic value-noise heightmap + trees) ----
fn hash01(x: i32, z: i32) -> f64 {
    let mut h = (x as u32).wrapping_mul(374761393) ^ (z as u32).wrapping_mul(668265263);
    h = (h ^ (h >> 13)).wrapping_mul(1274126177);
    ((h ^ (h >> 16)) & 0xffff) as f64 / 65535.0
}
fn smooth(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}
fn vnoise(x: f64, z: f64, cell: i32) -> f64 {
    let gx = (x / cell as f64).floor() as i32;
    let gz = (z / cell as f64).floor() as i32;
    let fx = smooth(x / cell as f64 - gx as f64);
    let fz = smooth(z / cell as f64 - gz as f64);
    let a = hash01(gx, gz);
    let b = hash01(gx + 1, gz);
    let c = hash01(gx, gz + 1);
    let d = hash01(gx + 1, gz + 1);
    a + (b - a) * fx + (c - a) * fz + (a - b - c + d) * fx * fz
}
// surface height: gentle hills, roughly 8..=22
fn height_at(x: i32, z: i32) -> i32 {
    if worldgen::enabled() {
        return worldgen::chunk_cols(x >> 4, z >> 4)
            .map(|c| c.surface((x & 15) as usize, (z & 15) as usize))
            .unwrap_or(15);
    }
    (8.0 + vnoise(x as f64, z as f64, 48) * 10.0
        + vnoise(x as f64 + 1000.0, z as f64 - 500.0, 12) * 4.0) as i32
}

/// The surface height a delivery for `dim` must use for anti-xray's vertical neighbours.
/// The Overworld arm is the long-standing `height_at`, byte for byte; a Nether or End
/// chunk asks that dimension's generator and, before a column exists, starts at its own
/// `min_y` rather than the Overworld's -64.
fn height_at_dim(dim: DimId, x: i32, z: i32) -> i32 {
    if dim == DimId::Overworld {
        return height_at(x, z);
    }
    // A Flat/Void custom dimension's surface is its layer stack, no generator needed.
    if let Some(h) = worldgen::custom_surface_height(dim) {
        return h;
    }
    if worldgen::enabled() {
        return worldgen::chunk_cols_dim(dim, x >> 4, z >> 4)
            .map(|c| c.surface((x & 15) as usize, (z & 15) as usize))
            .unwrap_or_else(|| dim.min_y());
    }
    height_at(x, z)
}

fn tree_at(x: i32, z: i32) -> bool {
    hash01(x ^ 0x5d3f, z ^ 0x7a1b) < 0.006
}
fn column_state(x: i32, y: i32, z: i32, h: i32) -> u32 {
    if y >= h {
        // trunk / leaves live above ground
        if tree_at(x, z) && y < h + 5 {
            return name_to_state("oak_log").unwrap_or(98);
        }
        if y <= 10 && h <= 10 {
            return name_to_state("water").unwrap_or(98);
        }
        if y <= h + 6 {
            for dx in -2i32..=2 {
                for dz in -2i32..=2 {
                    if dx == 0 && dz == 0 {
                        continue;
                    }
                    let (tx, tz) = (x + dx, z + dz);
                    if tree_at(tx, tz) {
                        let th = height_at(tx, tz);
                        if y >= th + 3 && y <= th + 5 {
                            return name_to_state("oak_leaves").unwrap_or(98);
                        }
                    }
                }
            }
        }
        return 0;
    }
    if h <= 11 {
        // shore floors under/near waterline
        if y < h - 2 {
            ore_state(x, y, z)
        } else {
            name_to_state("sand").unwrap_or(10)
        }
    } else if y < h - 4 {
        ore_state(x, y, z)
    } else if y < h - 1 {
        name_to_state("dirt").unwrap_or(10)
    } else {
        name_to_state("grass_block").unwrap_or(9)
    }
}
// datapack worldgen (worldgen.rs): vanilla density pipeline
fn wg2_block(x: i32, y: i32, z: i32) -> Option<u32> {
    // One lock for the whole decision (feature layer -> material window ->
    // aquifer -> deep rules); the five free accessors each took the lock again.
    worldgen::block_state(x, y, z, feature::populate)
}

fn door_base(st: u32) -> Option<u32> {
    for base in [
        4590u32, 11822, 11886, 11950, 12014, 12078, 12142, 12206, 12270, 19148, 19212, 23652,
        23716, 23780, 23844, 23908, 23972, 24036, 24100,
    ] {
        if (base..base + 64).contains(&st) {
            return Some(base);
        }
    }
    None
}
fn gate_base(st: u32) -> Option<u32> {
    for base in [
        6997u32, 11310, 11342, 11374, 11406, 11438, 11470, 11502, 11534, 18876, 18908,
    ] {
        if (base..base + 32).contains(&st) {
            return Some(base);
        }
    }
    None
}
fn trapdoor_state(st: u32) -> bool {
    breg().is_some_and(|reg| reg.name_of(st).ends_with("_trapdoor"))
}
fn is_fence_or_wall(st: u32) -> bool {
    matches!(st,
          5817..=5848 | 7273..=7304 | 11566..=11821 | 18684..=18747        // fences
        | 7919..=8566 | 14160..=18371 | 19541..=22314 | 24994..=26550)   // walls
        || (gate_base(st).is_some() && st & 2 != 0) // closed fence gate (1.5-high)
}
fn mob_passable(st: u32) -> bool {
    // a block a mob body can occupy
    if st == 0 || st == 12958 || st == 12959 {
        return true;
    } // air / void / cave
    if (80..=111).contains(&st) {
        return true;
    } // water / lava
    if is_fence_or_wall(st) {
        return false;
    }
    if gate_base(st).is_some() || door_base(st).is_some() {
        return st & 2 == 0;
    } // open gate/door
    matches!(st,
          25..=78 | 2004..=2010 | 2075..=2090 | 2355..=2871
        | 2978..=4273 | 4278..=4285 | 4302..=4589 | 4654..=4681
        | 5626..=5651 | 5716..=5733 | 5738..=5779 | 5799..=5814
        | 6821..=6996 | 7271..=7271 | 7385..=7388 | 7521..=7664
        | 8595..=8826 | 10728..=10743 | 10747..=10758 | 10759..=11078
        | 12340..=12409 | 12495..=12512 | 12760..=12786
        | 12933..=12945 | 12960..=12961 | 18503..=18542 | 18575..=18578
        | 18594..=18665 | 19100..=19147 | 20374..=20397 | 22318
        | 22800..=22927 | 24748..=24901 | 26572..=26572)
}
fn mob_cell_solid(x: f64, y: f64, z: f64) -> bool {
    // feet cell solid?
    let [cx, cy, cz] = [x, y, z].map(|v| v.floor() as i32);
    !mob_passable(block_at(cx, cy, cz))
}
fn ore_state(x: i32, y: i32, z: i32) -> u32 {
    let oh = ((x as i64).wrapping_mul(0x9E3779B1)
        ^ (z as i64).wrapping_mul(0x85EBCA77)
        ^ (y as i64).wrapping_mul(0xC2B2AE3D)) as u64;
    let oh = oh.wrapping_mul(0x27D4EB2F) ^ (oh >> 15);
    if (-60..=-52).contains(&y) && oh.is_multiple_of(23) {
        return name_to_state("lava").unwrap_or(1);
    }
    if y <= -32 && oh.is_multiple_of(601) {
        name_to_state("diamond_ore").unwrap_or(1)
    } else if y <= 4 && oh.is_multiple_of(197) {
        name_to_state("iron_ore").unwrap_or(1)
    } else if oh.is_multiple_of(89) {
        name_to_state("coal_ore").unwrap_or(1)
    } else {
        name_to_state("stone").unwrap_or(1)
    }
}
fn base_state(x: i32, y: i32, z: i32) -> u32 {
    if let Some(st) = wg2_block(x, y, z) {
        return st;
    }
    column_state(x, y, z, height_at(x, z))
}

/// `base_state` with the column height supplied. `height_at` is the expensive part - it is
/// where the terrain noise is read - and a pass that visits thousands of cells in a small area
/// asks for the same few hundred heights over and over. Measured: the block light spread spent
/// 449 of its 478 microseconds inside this lookup.
fn base_state_h(x: i32, y: i32, z: i32, h: i32) -> u32 {
    if let Some(st) = wg2_block(x, y, z) {
        return st;
    }
    column_state(x, y, z, h)
}
// ---------- view-gated world ----------
// hidden blocks (Paper anti-xray engine-mode-1 analogue): sent as plain stone
// until a face is exposed to a transparent neighbor — cuts palette diversity
// AND defeats x-ray, since buried ore never reaches the wire.
static HIDDEN: OnceLock<HashSet<u32>> = OnceLock::new();
static EMPTY_HIDDEN: OnceLock<HashSet<u32>> = OnceLock::new();
fn anti_xray_value_enabled(value: &str) -> bool {
    !matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no")
}
/// `[anticheat] antixray` in `server.toml` (default: on); `MC_SPINE_ANTI_XRAY` overrides.
fn anti_xray_enabled() -> bool {
    ac_anti_xray_enabled()
}
/// Blocks hidden from clients until seen (anti-xray). Named here rather than inline
/// so the list has one owner and a test can require every name to resolve: the
/// names used to be resolved with `filter_map`, which drops a misspelling silently
/// and leaves that ore visible to clients with nothing reported anywhere.
const HIDDEN_BLOCK_NAMES: &[&str] = &[
    "coal_ore",
    "iron_ore",
    "gold_ore",
    "diamond_ore",
    "lapis_ore",
    "redstone_ore",
    "copper_ore",
    "emerald_ore",
    "deepslate_coal_ore",
    "deepslate_iron_ore",
    "deepslate_diamond_ore",
    "deepslate_gold_ore",
    "deepslate_lapis_ore",
    "deepslate_redstone_ore",
    "deepslate_copper_ore",
    "deepslate_emerald_ore",
    "nether_gold_ore",
    "nether_quartz_ore",
    "ancient_debris",
    "chest",
    "trapped_chest",
    "barrel",
    "ender_chest",
    "spawner",
    "trial_spawner",
    "vault",
    "end_portal_frame",
    "budding_amethyst",
    "suspicious_sand",
    "suspicious_gravel",
    "reinforced_deepslate",
];
/// Every state of every hidden block, from the registry: a lit redstone ore or a chest
/// facing east is as much a give-away as the default state the names resolve to.
fn hidden_states() -> &'static HashSet<u32> {
    HIDDEN.get_or_init(|| {
        // Load the environment's extra ids now, so `is_hidden` knows whether to look.
        if !hidden_extra().lock().unwrap().is_empty() {
            HIDDEN_EXTRA_USED.store(true, Ordering::Relaxed);
        }
        let mut set: HashSet<u32> = HIDDEN_BLOCK_NAMES
            .iter()
            .filter_map(|n| name_to_state(n))
            .collect();
        if let Some(reg) = breg() {
            for st in 0..reg.state_count() as u32 {
                let name = reg.name_of(st);
                let name = name.strip_prefix("minecraft:").unwrap_or(name);
                if HIDDEN_BLOCK_NAMES.contains(&name) {
                    set.insert(st);
                }
            }
        }
        set
    })
}
static HIDDEN_EXTRA: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
fn hidden_extra() -> &'static Mutex<HashSet<u32>> {
    HIDDEN_EXTRA.get_or_init(|| {
        Mutex::new(
            std::env::var("MC_SPINE_HIDDEN_EXTRA")
                .unwrap_or_default()
                .split(',')
                .filter_map(|t| t.trim().parse::<u32>().ok())
                .collect(),
        )
    })
}
/// Set once anything is added to `hidden_extra`, so the common case skips its mutex.
static HIDDEN_EXTRA_USED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Automatically discover modded ores and containers tagged under standard
/// conventional tags (`#c:ores`, `#forge:ores`, `#c:chests`, `#forge:chests`,
/// `#c:barrels`, `#forge:barrels`, plus sub-tags like `#c:ores/zinc`) from
/// datapacks/mods and register every state into `hidden_extra()` so Anti-Xray
/// hides custom mod ores automatically with zero manual config.
fn register_hidden_tags_from_dir(root: &std::path::Path) -> usize {
    let data_dir = root.join("data");
    if !data_dir.is_dir() {
        return 0;
    }
    let mut tag_entries: HashMap<String, Vec<String>> = HashMap::new();
    let mut stack = vec![data_dir.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&data_dir) else {
                continue;
            };
            let parts: Vec<String> = rel
                .iter()
                .map(|c| c.to_string_lossy().into_owned())
                .collect();
            // data/<ns>/tags/block/<tag>.json OR data/<ns>/tags/blocks/<tag>.json
            if parts.len() < 4 || parts[1] != "tags" || (parts[2] != "block" && parts[2] != "blocks")
            {
                continue;
            }
            let ns = &parts[0];
            let stem_joined = parts[3..].join("/");
            let Some(stem) = stem_joined.strip_suffix(".json") else {
                continue;
            };
            let tag_key = format!("{ns}:{stem}");
            let Ok(raw) = std::fs::read(&path) else {
                continue;
            };
            let Some(J::Obj(obj)) = jparse(&raw) else {
                continue;
            };
            let Some((_, J::Arr(values))) = obj.iter().find(|(k, _)| k == "values") else {
                continue;
            };
            let list = tag_entries.entry(tag_key).or_default();
            for item in values {
                match item {
                    J::Str(s) => list.push(s.clone()),
                    J::Obj(map) => {
                        if let Some((_, J::Str(id))) = map.iter().find(|(k, _)| k == "id") {
                            list.push(id.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if tag_entries.is_empty() {
        return 0;
    }

    let is_hidden_root_tag = |tag: &str| {
        matches!(
            tag,
            "c:ores"
                | "forge:ores"
                | "c:chests"
                | "forge:chests"
                | "c:barrels"
                | "forge:barrels"
        ) || tag.starts_with("c:ores/")
            || tag.starts_with("forge:ores/")
            || tag.ends_with(":ores")
    };

    let mut queue: Vec<String> = tag_entries
        .keys()
        .filter(|k| is_hidden_root_tag(k))
        .cloned()
        .collect();
    let mut visited_tags: HashSet<String> = queue.iter().cloned().collect();
    let mut block_ids: HashSet<String> = HashSet::new();
    while let Some(tag) = queue.pop() {
        let Some(entries) = tag_entries.get(&tag) else {
            continue;
        };
        for entry in entries {
            if let Some(sub) = entry.strip_prefix('#') {
                let normalized = if sub.contains(':') {
                    sub.to_owned()
                } else {
                    format!("minecraft:{sub}")
                };
                if visited_tags.insert(normalized.clone()) {
                    queue.push(normalized);
                }
            } else {
                let normalized = if entry.contains(':') {
                    entry.clone()
                } else {
                    format!("minecraft:{entry}")
                };
                block_ids.insert(normalized);
            }
        }
    }

    let Some(reg) = breg() else {
        return 0;
    };
    let mut added = 0usize;
    let mut extra = hidden_extra().lock().unwrap();
    for st in 0..reg.state_count() as u32 {
        let name = reg.name_of(st);
        if block_ids.contains(name) && extra.insert(st) {
            added += 1;
        }
    }
    if added > 0 {
        HIDDEN_EXTRA_USED.store(true, Ordering::Relaxed);
    }
    added
}

fn refresh_hidden_tags_from_datapacks(datapacks_root: &std::path::Path) {
    let mut total = register_hidden_tags_from_dir(datapacks_root);
    if let Ok(rd) = std::fs::read_dir(datapacks_root) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                total += register_hidden_tags_from_dir(&p);
            }
        }
    }
    if total > 0 {
        eprintln!("[anticheat] auto-hid {total} block state(s) from #c:ores / #forge:ores / #c:chests tags");
    }
}

fn is_hidden(st: u32) -> bool {
    anti_xray_enabled()
        && (hidden_states().contains(&st)
            || (HIDDEN_EXTRA_USED.load(Ordering::Relaxed)
                && hidden_extra().lock().unwrap().contains(&st)))
}

/// `is_hidden` for a whole chunk's encode loop: the same answer without taking the
/// `hidden_extra` mutex per cell (the loop asks 36,864 times per chunk) and without
/// re-resolving the `OnceLock` each time.
struct HiddenSet {
    base: &'static HashSet<u32>,
    extra: HashSet<u32>,
}
impl HiddenSet {
    fn new() -> HiddenSet {
        if !anti_xray_enabled() {
            return HiddenSet {
                base: EMPTY_HIDDEN.get_or_init(HashSet::new),
                extra: HashSet::new(),
            };
        }
        HiddenSet {
            base: hidden_states(),
            extra: hidden_extra().lock().unwrap().clone(),
        }
    }
    fn contains(&self, st: u32) -> bool {
        self.base.contains(&st) || self.extra.contains(&st)
    }
}
/// Whether a neighbour lets a client see the face of the cell next to it: anything that
/// is not a full opaque cube (`isSolidRender` from the registry: air, fluids, glass and
/// tinted glass, leaves, slabs, torches, ...). A hidden neighbour does not count: unless
/// it is itself exposed it reaches the wire as solid filler.
fn exposes(st: u32) -> bool {
    match breg() {
        Some(reg) => !reg.is_solid_render(st) && !is_hidden(st),
        None => st == 0,
    }
}

/// The rock a buried hidden block is disguised as when none of its neighbours can vote.
fn host_rock(dim: DimId, x: i32, y: i32, z: i32, h: i32) -> u32 {
    let solid = |st: u32| !exposes(st) && !is_hidden(st);
    if dim == DimId::Nether {
        name_to_state("netherrack").unwrap_or(1)
    } else if dim == DimId::End {
        name_to_state("end_stone").unwrap_or(1)
    } else {
        let st = column_state(x, y, z, h);
        if solid(st) {
            st
        } else if y < 0 {
            name_to_state("deepslate").unwrap_or(1)
        } else {
            name_to_state("stone").unwrap_or(1)
        }
    }
}

// Context-matched filler: majority vote over the six neighbours, so the disguise matches
// the strata or the player-built wall around the cell. Only plain opaque neighbours vote.
fn hidden_filler_dim(dim: DimId, x: i32, y: i32, z: i32, h: i32) -> u32 {
    hidden_filler_with(
        &mut |a: i32, b: i32, c: i32| block_at_dim(dim, a, b, c),
        dim,
        x,
        y,
        z,
        h,
    )
}
// `rd` supplies block reads — chunk_data already holds world() and passes
// its own reader; taking the lock here would self-deadlock.
fn hidden_filler_with<F: FnMut(i32, i32, i32) -> u32>(
    rd: &mut F,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    h: i32,
) -> u32 {
    let mut counts: [(u32, u32); 6] = [(0, 0); 6];
    let mut len = 0usize;
    for (dx, dy, dz) in [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ] {
        let w = rd(x + dx, y + dy, z + dz);
        if is_hidden(w) || exposes(w) {
            continue;
        }
        if let Some(entry) = counts[..len].iter_mut().find(|(st, _)| *st == w) {
            entry.1 += 1;
        } else {
            counts[len] = (w, 1);
            len += 1;
        }
    }
    counts[..len]
        .iter()
        .max_by_key(|(st, n)| (*n, std::cmp::Reverse(*st)))
        .map(|(st, _)| *st)
        .unwrap_or_else(|| host_rock(dim, x, y, z, h))
}
/// The Overworld column height `host_rock` reads; other dimensions do not use it.
fn filler_height(dim: DimId, x: i32, z: i32) -> i32 {
    if dim == DimId::Overworld {
        height_at(x, z)
    } else {
        0
    }
}
fn block_exposed_dim(dim: DimId, x: i32, y: i32, z: i32) -> bool {
    [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ]
    .iter()
    .any(|(dx, dy, dz)| exposes(block_at_dim(dim, x + dx, y + dy, z + dz)))
}
// re-resolve every hidden neighbor's wire state after ANY mutation:
// exposed -> true state, still-buried -> context filler (re-hide).
fn refresh_hidden_neighbors(x: i32, y: i32, z: i32) {
    refresh_hidden_neighbors_dim(DimId::Overworld, x, y, z)
}
fn refresh_hidden_neighbors_dim(dim: DimId, x: i32, y: i32, z: i32) {
    for (dx, dy, dz) in [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ] {
        let (nx, ny, nz) = (x + dx, y + dy, z + dz);
        let ns = block_at_dim(dim, nx, ny, nz);
        if is_hidden(ns) {
            let out = if block_exposed_dim(dim, nx, ny, nz) {
                ns
            } else {
                hidden_filler_dim(dim, nx, ny, nz, filler_height(dim, nx, nz))
            };
            note_block_update_dim(dim, nx, ny, nz, out);
        }
    }
}
// hot region: chunks ticked by observers (players + actors) plus forceloaded
// chunks — everything outside sleeps, so idle CPU ~0 and mechanics outside
// view distance don't burn cycles (lazy-tick generalization).
/// The tick's view of which chunks are in scope, in one owner.
///
/// `refresh_hot` builds it once at the start of a tick and every system reads
/// that same set. Two things follow. A membership test costs a set lookup with
/// no lock, so a loop over N items acquires the mutex once instead of N times;
/// measuring the furnace loop showed that acquisition, not the lookup, was the
/// cost. And no caller can observe the region changing between two membership
/// tests inside one tick, because only `refresh_hot` writes it and it runs
/// first.
#[derive(Default)]
pub struct Region {
    hot: HashSet<(i32, i32)>,
    warm: HashSet<(i32, i32)>,
}
impl Region {
    fn is_hot(&self, x: i32, z: i32) -> bool {
        self.hot.contains(&(x.div_euclid(16), z.div_euclid(16)))
    }
    fn is_warm(&self, x: i32, z: i32) -> bool {
        self.warm.contains(&(x.div_euclid(16), z.div_euclid(16)))
    }
}
static REGION: OnceLock<Mutex<Region>> = OnceLock::new();
fn region() -> &'static Mutex<Region> {
    REGION.get_or_init(|| Mutex::new(Region::default()))
}
/// Borrow the region for the duration of one loop. This is the shape a per-item
/// membership test in a hot loop should use.
fn region_guard() -> TimedGuard<Region> {
    timed_region()
}
fn scheduled_tick_chunks() -> HashSet<(i32, i32)> {
    let __t = tickprof::enabled().then(std::time::Instant::now);
    let mut chunks = region_guard().hot.clone();
    if let Some(t) = __t {
        tickprof::add("sched/hot_clone", t.elapsed());
        tickprof::HOT_N.fetch_add(chunks.len() as u64, std::sync::atomic::Ordering::Relaxed);
    }
    let __tf = tickprof::enabled().then(std::time::Instant::now);
    let forced: Vec<(i32, i32)> = forceloaded().lock().unwrap().iter().copied().collect();
    for chunk in &forced {
        extend_chunk_square(&mut chunks, *chunk, 1);
    }
    if let Some(t) = __tf {
        tickprof::add("sched/forceload_expand", t.elapsed());
        tickprof::FORCED_N.fetch_add(forced.len() as u64, std::sync::atomic::Ordering::Relaxed);
    }
    chunks
}
fn extend_chunk_square(chunks: &mut HashSet<(i32, i32)>, (cx, cz): (i32, i32), radius: i32) {
    for dx in -radius..=radius {
        for dz in -radius..=radius {
            chunks.insert((cx + dx, cz + dz));
        }
    }
}
fn refresh_hot() {
    // collect without holding the hot lock: is_hot callers may hold pos
    // locks, so hot must never block on pos underneath itself.
    let mut h = forceloaded().lock().unwrap().clone();
    let forced: Vec<_> = h.iter().copied().collect();
    let mut centers = HashSet::new();
    for c in clients().lock().unwrap().iter() {
        let p = *c.pos.lock().unwrap();
        centers.insert((
            (p.0.floor() as i32).div_euclid(16),
            (p.2.floor() as i32).div_euclid(16),
        ));
    }
    for a in actors().lock().unwrap().values() {
        let p = *a.pos.lock().unwrap();
        centers.insert((
            (p.0.floor() as i32).div_euclid(16),
            (p.2.floor() as i32).div_euclid(16),
        ));
    }
    // The smoother thins the serviced window under real overload:
    // `chunk_send_scale` shrinks the warm ring toward the hot core and
    // `extra_view_trim` takes another ring off the edge. At `FULL` this is
    // exactly radius 5 + 8 — nothing about the set moves until the stage says
    // the server is struggling.
    let budget = current_load_budget();
    let warm_radius =
        ((8.0 * budget.chunk_send_scale).round() as i32 - budget.extra_view_trim).max(5);
    let warm_cells = ((2 * warm_radius + 1) * (2 * warm_radius + 1)) as usize;
    let skirt = warm_radius - 5;
    let skirt_cells = ((2 * skirt + 1) * (2 * skirt + 1)) as usize;
    for &center in &centers {
        extend_chunk_square(&mut h, center, 5);
    }
    // Square dilation distributes over union: hot radius + skirt = warm radius.
    // Dense, overlapping centers can make direct expansion MORE work. Choose
    // between equivalent unions by their number of insertion attempts (17²/7²
    // at full rate).
    // Both branches pay the forced ⊕3 ring, so the comparison is just the
    // differing part: centers expanded whole, or the shared set dilated.
    let mut w;
    if centers.len().saturating_mul(warm_cells) < h.len().saturating_mul(skirt_cells) {
        w = HashSet::new();
        for center in forced {
            extend_chunk_square(&mut w, center, 3);
        }
        for center in centers {
            extend_chunk_square(&mut w, center, warm_radius);
        }
    } else {
        w = h.clone();
        for center in forced {
            extend_chunk_square(&mut w, center, 3);
        }
        for &center in &h {
            extend_chunk_square(&mut w, center, skirt);
        }
    }
    let mut r = region().lock().unwrap();
    r.hot = h;
    r.warm = w;
}
// warm ring: block-level systems tick, entities/projectiles stay frozen —
// the vanilla lazy-chunk analogue (blocks progress, entities don't). The production paths call
// region().is_warm directly; this wrapper exists for the tests that pin the cold-furnace rule.
#[cfg(test)]
fn is_warm(x: i32, z: i32) -> bool {
    region().lock().unwrap().is_warm(x, z)
}

// generalized lazy-schedule primitive (the Create-generalization core):
// a cell that produces no observable change doubles its re-check interval
// up to 80 game ticks (4 s); any neighbor block change invalidates it back
// to full rate. Event-driven reset + exponential backoff, not polling.
// The tick counts live in backoff_ticks; only the 4 s ceiling belongs here.
static BACKOFF: Registry<DimBlockPos, (u64, u32)> = OnceLock::new();
fn backoff() -> &'static LockedMap<DimBlockPos, (u64, u32)> {
    BACKOFF.get_or_init(|| Mutex::new(HashMap::new()))
}
static TICK_IDX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static BLOCK_TICKS: OnceLock<Mutex<scheduled_ticks::ScheduledTicks>> = OnceLock::new();
static BLOCK_TICK_ORDER: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
fn block_ticks() -> &'static Mutex<scheduled_ticks::ScheduledTicks> {
    BLOCK_TICKS.get_or_init(|| Mutex::new(scheduled_ticks::ScheduledTicks::new()))
}

const LEAF_NEIGHBORS: [(i32, i32, i32); 6] = [
    (0, -1, 0),
    (0, 1, 0),
    (0, 0, -1),
    (0, 0, 1),
    (-1, 0, 0),
    (1, 0, 0),
];

fn leaf_block(reg: &crate::block_registry::BReg, state: u32) -> Option<u32> {
    let name = reg.name_of(state);
    name.ends_with("_leaves").then(|| reg.state_of(name, None))
}

fn leaf_distance_update(reg: &crate::block_registry::BReg, state: u32, neighbors: [u32; 6]) -> u32 {
    let distance = neighbors
        .into_iter()
        .map(|n| reg.distance_from_log(n).unwrap_or(7) + 1)
        .min()
        .unwrap_or(7)
        .min(7);
    reg.with_prop(state, "distance", &distance.to_string())
}

/// LeavesBlock.updateShape is invoked for the neighbours of a changed block.
fn schedule_leaf_neighbors(x: i32, y: i32, z: i32, changed_state: u32) {
    let Some(reg) = crate::block_registry::breg() else {
        return;
    };
    let distance = reg.distance_from_log(changed_state).unwrap_or(7) + 1;
    for (dx, dy, dz) in LEAF_NEIGHBORS {
        let p = (x + dx, y + dy, z + dz);
        if p.1 < MIN_Y || p.1 >= MIN_Y + 384 {
            continue;
        }
        let st = block_at(p.0, p.1, p.2);
        let Some(block) = leaf_block(reg, st) else {
            continue;
        };
        if distance == 1 && reg.get_prop(st, "distance").as_deref() == Some("1") {
            continue;
        }
        let due = TICK_IDX.load(Ordering::Relaxed) as i64 + 1;
        let sub_order = BLOCK_TICK_ORDER.fetch_add(1, Ordering::Relaxed);
        block_ticks()
            .lock()
            .unwrap()
            .schedule(scheduled_ticks::ScheduledTick {
                pos: p,
                block,
                due,
                priority: 0,
                sub_order,
            });
    }
}

/// Read many block states with two lock acquisitions instead of one pair per cell.
///
/// `block_at` takes the world lock and, for a generated cell, the worldgen lock — per
/// call. `scheduled_block_tick` reads seven cells for every due leaf tick, and a live
/// server measured **2.76 M worldgen lock acquisitions in one 100-tick window** (27,568
/// per tick) with 16.8 ms of every tick spent waiting for that lock while the
/// connection thread's section passes hold it. This resolves the same cells with one
/// world-lock section (copying the touched chunks' edits) and one worldgen section.
///
/// The value per position is what `block_at` would return: the overlay wins, and
/// `block_state_at` (the primitive `base_state` uses) supplies the generated state.
fn read_states_batched(positions: &[(i32, i32, i32)]) -> Vec<u32> {
    let mut overlays: HashMap<(i32, i32), HashMap<u32, u32>> = HashMap::new();
    {
        let w = world().lock().unwrap();
        for &(x, _, z) in positions {
            let key = (x >> 4, z >> 4);
            if let std::collections::hash_map::Entry::Vacant(e) = overlays.entry(key) {
                if let Some(col) = overworld_edits(&w, key.0, key.1) {
                    e.insert(col.clone());
                }
            }
        }
    }
    let mut out = vec![0u32; positions.len()];
    // The chunk band first: it is the states `chunk_data` already evaluated, so a leaf's
    // neighbours cost an array read instead of a worldgen evaluation. Falls back to the
    // live overlay, then to a batched worldgen pass for cells the band does not cover.
    let mut unresolved: Vec<usize> = Vec::new();
    for (i, &(x, y, z)) in positions.iter().enumerate() {
        match band_state(x, y, z) {
            Some(st) => out[i] = st,
            None => unresolved.push(i),
        }
    }
    let generated: Vec<usize> = unresolved
        .into_iter()
        .filter(|&i| {
            let (x, y, z) = positions[i];
            overlays
                .get(&(x >> 4, z >> 4))
                .is_none_or(|c| !c.contains_key(&local_idx(x, y, z)))
        })
        .collect();
    if !generated.is_empty() {
        crate::worldgen::with_gen(|g| {
            for &i in &generated {
                let (x, y, z) = positions[i];
                out[i] = g.block_state_at(x, y, z, crate::feature::populate);
            }
        });
    }
    for (i, &(x, y, z)) in positions.iter().enumerate() {
        if let Some(col) = overlays.get(&(x >> 4, z >> 4)) {
            if let Some(&st) = col.get(&local_idx(x, y, z)) {
                out[i] = st;
            }
        }
    }
    out
}

fn scheduled_block_tick() {
    with_save_guard(scheduled_block_tick_inner);
}
fn scheduled_block_tick_inner() {
    let __prof = tickprof::enabled();
    let mut __t = __prof.then(std::time::Instant::now);
    let now = TICK_IDX.load(Ordering::Relaxed) as i64;
    // Its own lock: this drain used to take the worldgen lock, and a section's first cell
    // populates the chunk under that lock (tens of ms), so the tick thread waited behind
    // generation for a queue that has nothing to do with it.
    let generated = worldgen::take_generated_leaf_ticks();
    if let Some(t) = __t {
        tickprof::add("sched/take_generated", t.elapsed());
        tickprof::GEN_TICKS.fetch_add(generated.len() as u64, std::sync::atomic::Ordering::Relaxed);
    }
    __t = __prof.then(std::time::Instant::now);
    if !generated.is_empty() {
        block_ticks()
            .lock()
            .unwrap()
            .unpack_generated(generated, now);
    }
    if let Some(t) = __t {
        tickprof::add("sched/schedule_loop", t.elapsed());
    }
    __t = __prof.then(std::time::Instant::now);
    let eligible = scheduled_tick_chunks();
    if let Some(t) = __t {
        tickprof::add("sched/eligible", t.elapsed());
    }
    __t = __prof.then(std::time::Instant::now);
    let due = block_ticks()
        .lock()
        .unwrap()
        .collect(now, 65_536, |chunk| eligible.contains(&chunk));
    if let Some(t) = __t {
        tickprof::add("sched/collect", t.elapsed());
    }
    __t = __prof.then(std::time::Instant::now);
    let Some(reg) = crate::block_registry::breg() else {
        return;
    };
    // All the cells this pass reads, in one batch: the tick's own cell, then its six
    // neighbours (fixed layout, so the loop indexes it).
    const STRIDE: usize = 7;
    let mut positions: Vec<(i32, i32, i32)> = Vec::with_capacity(due.len() * STRIDE);
    for tick in &due {
        let (x, y, z) = tick.pos;
        positions.push(tick.pos);
        for (dx, dy, dz) in LEAF_NEIGHBORS {
            positions.push((x + dx, y + dy, z + dz));
        }
    }
    let states = read_states_batched(&positions);
    if let Some(t) = __t {
        tickprof::add("sched/read_batch", t.elapsed());
    }
    __t = __prof.then(std::time::Instant::now);
    for (i, tick) in due.iter().enumerate() {
        let (x, y, z) = tick.pos;
        let b = i * STRIDE;
        let st = states[b];
        if reg.behaviour_of(tick.block) == crate::block_registry::B_FALLING {
            // Native scheduled ticks hold a Block identity, not a complete
            // BlockState. Reorienting an anvil during the wait must not cancel
            // its pending callback; replacing it with another block does.
            if reg.behaviour_of(st) == crate::block_registry::B_FALLING
                && reg.name_of(st) == reg.name_of(tick.block)
                && fallable_state(states[b + 1]) {
                spawn_fall(x, y, z, st);
            }
            continue;
        }
        if leaf_block(reg, st) != Some(tick.block) {
            continue;
        }
        let neighbors = [
            states[b + 1],
            states[b + 2],
            states[b + 3],
            states[b + 4],
            states[b + 5],
            states[b + 6],
        ];
        let result = leaf_distance_update(reg, st, neighbors);
        if result != st && set_block_if(x, y, z, result, Some(st)) {
            note_block_update(x, y, z, result);
        }
    }
    if let Some(t) = __t {
        tickprof::add("sched/write_loop", t.elapsed());
    }
}

// Generated source-fluid reservations share the proto handoff, but only their
// supported fluid/shape/contact paths belong to this runtime owner.
thread_local! { static NATIVE_FLUID_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
// A fluid owner can write through set_block while it holds its queue mutex.
// Its neighbor notifications must not re-acquire that mutex. Defer just the
// waterlogged source positions and drain them after the owner releases it.
thread_local! {
    static WATERLOGGED_DEFER: std::cell::RefCell<Vec<BlockPos>> =
        const { std::cell::RefCell::new(Vec::new()) };
}
static GENERATED_FLUID_OWNER: OnceLock<Mutex<GeneratedFluidOwner>> = OnceLock::new();

struct GeneratedFluidOwner {
    queue: scheduled_ticks::ScheduledTicks,
    next_order: i64,
    owned: HashSet<BlockPos>,
    reported: HashSet<BlockPos>,
}

impl GeneratedFluidOwner {
    fn new() -> Self {
        Self {
            queue: scheduled_ticks::ScheduledTicks::new(),
            next_order: 0,
            owned: HashSet::new(),
            reported: HashSet::new(),
        }
    }
    fn unpack_generated(&mut self, pending: Vec<(BlockPos, u32)>, now: i64) {
        self.owned.extend(pending.iter().map(|&(pos, _)| pos));
        self.queue.unpack_generated(pending, now);
    }
    fn schedule(&mut self, pos: BlockPos, fluid: flowing_fluid::Fluid, now: i64, immediate: bool) {
        self.owned.insert(pos);
        let block = match (fluid.kind, fluid.source()) {
            (flowing_fluid::Kind::Water, true) => 0,
            (flowing_fluid::Kind::Water, false) => 1,
            (flowing_fluid::Kind::Lava, true) => 2,
            (flowing_fluid::Kind::Lava, false) => 3,
        };
        let delay = if immediate {
            0
        } else if fluid.kind == flowing_fluid::Kind::Water {
            5
        } else {
            30
        };
        self.queue.schedule(scheduled_ticks::ScheduledTick {
            pos,
            block,
            due: now + delay,
            priority: 0,
            sub_order: self.next_order,
        });
        self.next_order += 1;
    }
    fn write(&mut self, pos: BlockPos, cell: flowing_fluid::Cell, now: i64) -> Result<(), String> {
        let state = match cell {
            flowing_fluid::Cell::Air => 0,
            flowing_fluid::Cell::Fluid(f) => {
                (if f.kind == flowing_fluid::Kind::Water {
                    80
                } else {
                    96
                }) + u32::from(f.level)
            }
            // A plan replaces a `Passable` cell *with* the fluid it spreads, so neither of these
            // is reachable as a write; refusing them keeps the writer's contract honest.
            flowing_fluid::Cell::Solid | flowing_fluid::Cell::Passable => {
                return Err("fluid plan cannot write a non-fluid cell".into())
            }
        };
        self.write_state(pos, state, now);
        Ok(())
    }
    fn write_state(&mut self, pos: BlockPos, state: u32, now: i64) {
        let old = block_at(pos.0, pos.1, pos.2);
        let old_fluid = fluid_from_block(old);
        if old == state {
            return;
        }
        let previous = NATIVE_FLUID_WRITE.with(|v| v.replace(true));
        struct ResetNativeFluidWrite(bool);
        impl std::ops::Drop for ResetNativeFluidWrite {
            fn drop(&mut self) {
                NATIVE_FLUID_WRITE.with(|v| v.set(self.0));
            }
        }
        let reset = ResetNativeFluidWrite(previous);
        set_block(pos.0, pos.1, pos.2, state);
        drop(reset);
        let current = fluid_from_block(state);
        if current.is_none() {
            self.owned.remove(&pos);
        }
        if let Some(f) = current {
            if old_fluid.is_none_or(|old| old.kind != f.kind) {
                self.schedule(pos, f, now, false);
            }
        }
        for (dx, dy, dz) in LEAF_NEIGHBORS {
            let n = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
            if n.1 < MIN_Y || n.1 >= MIN_Y + 384 {
                continue;
            }
            let nst = block_at(n.0, n.1, n.2);
            if let Some(f) = fluid_from_block(nst) {
                if f.source() || current.is_some_and(|f| f.source()) {
                    self.schedule(n, f, now, false);
                }
            } else if waterlogged_source(nst) {
                WATERLOGGED_DEFER.with(|v| v.borrow_mut().push(n));
            }
        }
    }
    fn tick(&mut self, now: i64, eligible: &HashSet<(i32, i32)>) -> Result<(), String> {
        let due = self
            .queue
            .collect(now, 65_536, |chunk| eligible.contains(&chunk));
        let mut first_error = None;
        for event in due {
            let kind = if event.block < 2 {
                flowing_fluid::Kind::Water
            } else {
                flowing_fluid::Kind::Lava
            };
            let source = event.block.is_multiple_of(2);
            let plan = match flowing_fluid::tick(
                &mut FluidReader,
                event.pos,
                kind,
                source,
                source_conversion_enabled(kind),
            ) {
                Ok(plan) => {
                    self.reported.remove(&event.pos);
                    plan
                }
                Err(error) => {
                    self.queue.schedule(scheduled_ticks::ScheduledTick {
                        due: now + 1,
                        ..event
                    });
                    if first_error.is_none() && self.reported.insert(event.pos) {
                        first_error = Some(error);
                    }
                    continue;
                }
            };
            for (pos, cell) in plan.writes {
                self.write(pos, cell, now)?;
                if pos == event.pos {
                    if let Some(f) = plan.updated_self {
                        self.schedule(pos, f, now, false);
                    }
                }
            }
            for (pos, reaction) in plan.solidifications {
                if let Some(state) = fluid_solidification_state(reaction) {
                    self.write_state(pos, state, now);
                }
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }
}

fn generated_fluid_tick() {
    with_save_guard(generated_fluid_tick_inner);
}
fn generated_fluid_tick_inner() {
    let now = TICK_IDX.load(Ordering::Relaxed) as i64;
    let generated = worldgen::take_generated_fluid_ticks();
    if generated.is_empty() && GENERATED_FLUID_OWNER.get().is_none() {
        return;
    }
    let eligible = scheduled_tick_chunks();
    let mut owner = GENERATED_FLUID_OWNER
        .get_or_init(|| Mutex::new(GeneratedFluidOwner::new()))
        .lock()
        .unwrap();
    owner.unpack_generated(generated, now);
    if let Err(error) = owner.tick(now, &eligible) {
        eprintln!("[fluid] deferred native spring tick: {error}");
    }
    drop(owner);
    // The collector's own writes can wake adjacent waterlogged sources.
    // Schedule them for the next water tick *after* leaving the owner lock.
    for p in WATERLOGGED_DEFER.with(|v| std::mem::take(&mut *v.borrow_mut())) {
        schedule_waterlogged_source(p);
    }
}

fn generated_fluid_changed(pos: BlockPos, old: u32, state: u32) -> bool {
    if NATIVE_FLUID_WRITE.with(|v| v.get()) {
        return true;
    }
    let Some(owner) = GENERATED_FLUID_OWNER.get() else {
        return false;
    };
    let mut owner = owner.lock().unwrap();
    let controlled = owner.owned.contains(&pos);
    if old == state {
        return controlled;
    }
    let current = fluid_from_block(state);
    let now = TICK_IDX.load(Ordering::Relaxed) as i64;
    if controlled {
        if let Some(fluid) = current {
            owner.schedule(pos, fluid, now, false);
        } else {
            owner.owned.remove(&pos);
        }
    }
    for (dx, dy, dz) in LEAF_NEIGHBORS {
        let n = (pos.0 + dx, pos.1 + dy, pos.2 + dz);
        if !owner.owned.contains(&n) {
            continue;
        }
        if let Some(fluid) = fluid_from_block(block_at(n.0, n.1, n.2)) {
            if fluid.source() || current.is_some_and(|f| f.source()) {
                owner.schedule(n, fluid, now, false);
            }
        }
    }
    controlled
}

fn fluid_from_block(st: u32) -> Option<flowing_fluid::Fluid> {
    if (80..=95).contains(&st) {
        Some(flowing_fluid::Fluid {
            kind: flowing_fluid::Kind::Water,
            level: (st - 80) as u8,
        })
    } else if (96..=111).contains(&st) {
        Some(flowing_fluid::Fluid {
            kind: flowing_fluid::Kind::Lava,
            level: (st - 96) as u8,
        })
    } else {
        None
    }
}

/// SimpleWaterloggedBlock reports a WATER source without becoming LiquidBlock.
fn waterlogged_source(st: u32) -> bool {
    let Some(reg) = crate::block_registry::breg() else { return false };
    reg.has_water(st)
        && reg.get_prop(st, "waterlogged").as_deref() == Some("true")
        && reg.fluid_name(st) == "minecraft:water"
}

fn schedule_waterlogged_source(p: BlockPos) {
    if !waterlogged_source(block_at(p.0, p.1, p.2)) {
        return;
    }
    let now = TICK_IDX.load(Ordering::Relaxed) as i64;
    let owner = GENERATED_FLUID_OWNER.get_or_init(|| Mutex::new(GeneratedFluidOwner::new()));
    owner.lock().unwrap().schedule(
        p,
        flowing_fluid::Fluid { kind: flowing_fluid::Kind::Water, level: 0 },
        now,
        false, // Native SimpleWaterloggedBlock books a water tick 5 GT later.
    );
}

/// Schedule already-read waterlogged sources. Do not re-read the six neighbor
/// cells: the shape-update visitor already does that for every block write.
/// Fluid-owned writes defer until the owner lock is released.
fn schedule_waterlogged_positions(positions: Vec<BlockPos>) {
    if NATIVE_FLUID_WRITE.with(|v| v.get()) {
        WATERLOGGED_DEFER.with(|v| v.borrow_mut().extend(positions));
    } else {
        for p in positions {
            schedule_waterlogged_source(p);
        }
    }
}

/// `FluidState.tick` for the chunk-promotion pass (`LevelChunk.postProcessGeneration`'s
/// `if (!fs.isEmpty()) fs.tick(this.level, p)`), bridged onto `flowing_fluid`.
///
/// `structure_template` is Layer 4 and `flowing_fluid` is Layer 3, so the machine cannot be
/// reached from the pass: `main` registers this function into it at startup (and in the
/// tests), the same seam `feature::register_beard_resolver` uses for the terrain layer.
struct PassFluidReader<'a>(&'a mut dyn structure_template::FluidTickTarget);

impl flowing_fluid::Reader for PassFluidReader<'_> {
    // The refusal names the cell it could not answer, so a trace can say *which* neighbour
    // the machine stopped on instead of only that it stopped.
    type Error = flowing_fluid::Pos;
    fn cell(&mut self, pos: flowing_fluid::Pos) -> Result<flowing_fluid::Cell, flowing_fluid::Pos> {
        match self.0.fluid_cell(pos) {
            Some(structure_template::FluidCell::Air) => Ok(flowing_fluid::Cell::Air),
            Some(structure_template::FluidCell::Solid) => Ok(flowing_fluid::Cell::Solid),
            Some(structure_template::FluidCell::Passable) => Ok(flowing_fluid::Cell::Passable),
            Some(structure_template::FluidCell::Fluid { water, level }) => {
                Ok(flowing_fluid::Cell::Fluid(flowing_fluid::Fluid {
                    kind: if water {
                        flowing_fluid::Kind::Water
                    } else {
                        flowing_fluid::Kind::Lava
                    },
                    level,
                }))
            }
            // Vanilla's `FlowingFluid` has no such answer: a non-empty partial collision shape
            // or a cross-fluid contact is a case this machine refuses rather than guesses. The
            // pass then leaves the cell alone, which is the fail-closed half of the contract
            // on `flowing_fluid::Reader`.
            None => Err(pos),
        }
    }
    fn unsupported_contact(&mut self, pos: flowing_fluid::Pos) -> flowing_fluid::Pos {
        pos
    }
}

/// `FluidState.tick(level, pos)` at one marked cell. `expected_source` is the cell's own
/// source-ness (so a marked flowing cell takes the `getNewLiquid` arm and a source goes
/// straight to `spread`, which is vanilla's split), and `source_conversion` is the live
/// `waterSourceConversion`/`lavaSourceConversion` gamerule per kind, matching
/// `FlowableFluid`'s `canConvertToSource` as the runtime scheduler already reads it.
fn promotion_fluid_tick(
    world: &mut dyn structure_template::FluidTickTarget,
    pos: flowing_fluid::Pos,
) {
    let Some(structure_template::FluidCell::Fluid { water, level }) = world.fluid_cell(pos) else {
        return;
    };
    let kind = if water {
        flowing_fluid::Kind::Water
    } else {
        flowing_fluid::Kind::Lava
    };
    let plan = {
        let mut reader = PassFluidReader(&mut *world);
        flowing_fluid::tick(&mut reader, pos, kind, level == 0, source_conversion_enabled(kind))
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(blocked) => {
            // `flowing_fluid` refuses a cell it has no native contract for (a non-full
            // collision shape, a cross-fluid contact). Vanilla would have an answer; this
            // port leaves the cell alone rather than invent one, and says so when asked.
            if std::env::var_os("FLUID_TICK_TRACE").is_some() {
                // Position only: this runs inside `worldgen::with_gen`, and `block_at`
                // re-enters the generator lock (the cell may not be in the runtime store),
                // which is a self-deadlock on the generator lock. The name is not worth
                // that: the machine's refusal is about the *cell*, and the cell is what is
                // printed.
                eprintln!("[fluid-tick] mark={pos:?} stopped on {blocked:?}");
            }
            return;
        }
    };
    if std::env::var_os("FLUID_TICK_TRACE").is_some() && !plan.writes.is_empty() {
        eprintln!(
            "[fluid-tick] {:?} writes={} self={:?}",
            pos,
            plan.writes.len(),
            plan.updated_self
        );
    }
    for (write_pos, cell) in plan.writes {
        // `Plan::writes` uses the machine's own cell type; `Solid` is never planned (a fluid
        // plan writes air or fluid), so an unreachable arm is the honest translation.
        let cell = match cell {
            flowing_fluid::Cell::Air => structure_template::FluidCell::Air,
            flowing_fluid::Cell::Fluid(f) => structure_template::FluidCell::Fluid {
                water: f.kind == flowing_fluid::Kind::Water,
                level: f.level,
            },
            // A plan writes air or fluid only (`spread_to` replaces a `Passable` cell *with* the
            // fluid), so these two arms are unreachable; skipping is the honest translation.
            flowing_fluid::Cell::Solid | flowing_fluid::Cell::Passable => continue,
        };
        world.set_fluid(write_pos, cell);
    }
}

struct FluidReader;
impl flowing_fluid::Reader for FluidReader {
    type Error = String;
    fn cell(&mut self, pos: BlockPos) -> Result<flowing_fluid::Cell, String> {
        if pos.1 < MIN_Y || pos.1 >= MIN_Y + 384 {
            return Err(format!("fluid read outside world at {pos:?}"));
        }
        let st = block_at(pos.0, pos.1, pos.2);
        if let Some(f) = fluid_from_block(st) {
            return Ok(flowing_fluid::Cell::Fluid(f));
        }
        if waterlogged_source(st) {
            return Ok(flowing_fluid::Cell::Fluid(flowing_fluid::Fluid {
                kind: flowing_fluid::Kind::Water,
                level: 0,
            }));
        }
        let reg = crate::block_registry::breg().ok_or("missing block registry")?;
        if reg.is_air(st) {
            Ok(flowing_fluid::Cell::Air)
        } else if fluid_washes_away(reg, st) {
            Ok(flowing_fluid::Cell::Passable)
        } else if reg.is_solid_render(st)
            || matches!(
                reg.name_of(st),
                "minecraft:chest" | "minecraft:trapped_chest" | "minecraft:ender_chest"
            )
        {
            Ok(flowing_fluid::Cell::Solid)
        } else {
            Err(format!(
                "unsupported fluid shape {} at {pos:?}",
                reg.name_of(st)
            ))
        }
    }
    fn unsupported_contact(&mut self, pos: BlockPos) -> String {
        format!("unsupported fluid contact at {pos:?}")
    }
}
fn sched_ready(x: i32, y: i32, z: i32) -> bool {
    let now = TICK_IDX.load(Ordering::Relaxed);
    backoff()
        .lock()
        .unwrap()
        .get(&(DimId::Overworld, x, y, z))
        .map(|(t, _)| *t <= now)
        .unwrap_or(true)
}
static LAVA_QUEUE: OnceLock<Mutex<tick_queue::TickQueue>> = OnceLock::new();
fn lava_queue() -> &'static Mutex<tick_queue::TickQueue> {
    LAVA_QUEUE.get_or_init(|| Mutex::new(tick_queue::TickQueue::new()))
}
/// Re-queue a due cell for its next visit. The delay comes from the backoff map,
/// so the ladder is unchanged, and a cell with no pending backoff is due next
/// invocation.
///
/// `period` is the number of ticks between this system's invocations: 1 for water
/// and hoppers, which run every tick, and `server_ticks(FLUID_TICK_GT)` for lava.
/// A cell can only ever be visited on those boundaries, so "due at tick T" and
/// "first visit at or after T" are the same statement and the rounding is exact
/// rather than an approximation. Getting this wrong for lava once made its delays
/// thirty times too long.
fn requeue_fluid(queue: &Mutex<tick_queue::TickQueue>, pos: (i32, i32, i32), period: u64) {
    let now = TICK_IDX.load(Ordering::Relaxed);
    let delay = match backoff()
        .lock()
        .unwrap()
        .get(&(DimId::Overworld, pos.0, pos.1, pos.2))
    {
        Some((due, _)) if *due > now => (*due - now).div_ceil(period) as usize,
        _ => 1,
    };
    queue.lock().unwrap().schedule(pos, delay);
}
fn requeue_lava(x: i32, y: i32, z: i32) {
    requeue_fluid(lava_queue(), (x, y, z), server_ticks(FLUID_TICK_GT))
}
static HOPPER_QUEUE: OnceLock<Mutex<tick_queue::TickQueue>> = OnceLock::new();
fn hopper_queue() -> &'static Mutex<tick_queue::TickQueue> {
    HOPPER_QUEUE.get_or_init(|| Mutex::new(tick_queue::TickQueue::new()))
}
fn requeue_hopper(x: i32, y: i32, z: i32) {
    requeue_fluid(hopper_queue(), (x, y, z), 1)
}
static WATER_QUEUE: OnceLock<Mutex<tick_queue::TickQueue>> = OnceLock::new();
fn water_queue() -> &'static Mutex<tick_queue::TickQueue> {
    WATER_QUEUE.get_or_init(|| Mutex::new(tick_queue::TickQueue::new()))
}
fn requeue_water(x: i32, y: i32, z: i32) {
    requeue_fluid(water_queue(), (x, y, z), 1)
}
/// call after evaluating a cell: did_work=true resets, false backs off.
fn sched_result(x: i32, y: i32, z: i32, did_work: bool) {
    let now = TICK_IDX.load(Ordering::Relaxed);
    let mut b = backoff().lock().unwrap();
    let key = (DimId::Overworld, x, y, z);
    if did_work {
        b.remove(&key);
    } else {
        let fails = b.get(&key).map(|e| e.1).unwrap_or(0) + 1;
        b.insert(key, (now + backoff_ticks(u64::from(fails)), fails));
    }
}
/// any world mutation resets backoff for the cell and its 6 neighbors —
/// the event-driven wake that makes lazy intervals safe.
/// The chunks a random tick may land in. The world is a sparse overlay of edits over
/// generated terrain, so there is no list of loaded chunks to walk; the hot region is the
/// existing definition of where the world is simulated, and forceload marks chunks
/// explicitly. No third notion of "loaded" is introduced.
fn ticked_chunks() -> Vec<(i32, i32)> {
    let mut out: Vec<(i32, i32)> = {
        let r = region_guard();
        r.hot.iter().copied().collect()
    };
    out.extend(forceloaded().lock().unwrap().iter().copied());
    out.sort_unstable();
    out.dedup();
    out
}

/// Sections a random tick may land in for this chunk: the ones it has edits in, plus the
/// band around its surface, which is where placed blocks sit.
fn ticked_sections(cx: i32, cz: i32) -> Vec<i32> {
    let mut out: Vec<i32> = Vec::new();
    if let Some(c) = world().lock().unwrap().get(&(DimId::Overworld, cx, cz)) {
        for idx in c.keys() {
            out.push(((*idx >> 8) as i32 + MIN_Y) >> 4);
        }
    }
    let surface = height_at(cx * 16 + 8, cz * 16 + 8);
    out.push((surface - 16) >> 4);
    out.push((surface + 4) >> 4);
    out.sort_unstable();
    out.dedup();
    out
}

/// One pass of vanilla's random block ticks. A position is drawn per section rather than
/// walking every block, so the cost follows the number of sections instead of the number
/// of blocks, and a position that holds nothing costs one array lookup. The `randomTickSpeed`
/// gamerule sets the number of samples per section; `0` skips the pass entirely.
fn random_tick_pass() {
    let speed = gr_random_tick_speed();
    if speed == 0 {
        return;
    }
    // The registry is looked up once per pass, not once per sample.
    let Some(reg) = crate::block_registry::breg() else {
        return;
    };
    for (cx, cz) in ticked_chunks() {
        for sy in ticked_sections(cx, cz) {
            for _ in 0..speed {
                let x = cx * 16 + world_rng::next_below(16) as i32;
                let y = sy * 16 + world_rng::next_below(16) as i32;
                let z = cz * 16 + world_rng::next_below(16) as i32;
                on_random_tick(reg, x, y, z);
            }
        }
    }
}

/// What a block does when a random tick lands on it. Vanilla calls the block's
/// `randomTick`; here the block is identified by name from the registry rather than by a
/// hand-written id range, so a block added from the asset needs no edit to this file.
/// `on_random_tick` keeps the Overworld-only signature existing callers and tests use;
/// the dimension-aware work lives in `on_random_tick_dim`.
fn on_random_tick(reg: &crate::block_registry::BReg, x: i32, y: i32, z: i32) {
    on_random_tick_dim(reg, DimId::Overworld, x, y, z)
}

fn on_random_tick_dim(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32) {
    let st = block_at_dim(dim, x, y, z);
    // Two cheap rejections cover the overwhelming share of sampled cells — air is
    // most of the volume and stone is most of the rest — so a string lookup runs
    // only for cells that might actually tick.
    if reg.is_air(st) || st == reg.default_state(1) {
        return;
    }
    // One compare against zero for the behaviour table's only random-tick id,
    // rather than a string compare against the block's name on every sample.
    if reg.behaviour_of(st) == crate::block_registry::B_SUGAR_CANE {
        sugar_cane_random_tick(reg, dim, x, y, z, st);
        return;
    }
    match reg.name_of(st) {
        "minecraft:wheat" | "minecraft:carrots" | "minecraft:potatoes"
        | "minecraft:beetroots" | "minecraft:nether_wart" | "minecraft:melon_stem"
        | "minecraft:pumpkin_stem" | "minecraft:sweet_berry_bush" | "minecraft:cocoa" => {
            crop_random_tick(reg, dim, x, y, z, st)
        }
        "minecraft:fire" | "minecraft:soul_fire" => fire_random_tick(reg, dim, x, y, z, st),
        "minecraft:ice" => ice_random_tick(reg, dim, x, y, z),
        "minecraft:water" => water_random_tick(reg, dim, x, y, z, st),
        "minecraft:snow" => snow_random_tick(reg, dim, x, y, z, st),
        "minecraft:grass_block" | "minecraft:mycelium" => {
            grass_random_tick(reg, dim, x, y, z, st)
        }
        // `bamboo_sapling` must reach its own handler before the `_sapling`
        // suffix arm claims it for the tree-grow tick.
        _ if p2_plant_random_tick(reg, dim, x, y, z, st) => {}
        name if name.ends_with("_leaves") => leaf_random_tick(reg, dim, x, y, z, st),
        name if name.ends_with("_sapling") => sapling_random_tick(reg, dim, x, y, z, st),
        _ => {}
    }
}

/// `Level.getBrightness(LightLayer.BLOCK, pos)` approximated from the block
/// registry: the cell's own emission and the strongest neighbour minus one step
/// of attenuation. The light engine does not index cells a tick wrote, so the
/// rules that want block light — ice melt, snow accumulation, fire spread —
/// read the emissions of the written state instead.
fn random_tick_block_light(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32) -> u8 {
    let mut light = reg.luminance_of(block_at_dim(dim, x, y, z));
    for (dx, dy, dz) in [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ] {
        let n = reg.luminance_of(block_at_dim(dim, x + dx, y + dy, z + dz));
        light = light.max(n.saturating_sub(1));
    }
    light
}

/// `Level.canSeeSky(pos)`: the cell sits at or above the surface the column
/// heightmap recorded, in a dimension with skylight.
fn random_tick_sky_exposed(dim: DimId, x: i32, y: i32, z: i32) -> bool {
    dim.has_skylight() && y >= height_at_dim(dim, x, z)
}

/// `Level.getMaxLocalRawBrightness`: block light combined with full sky light
/// where the column is exposed.
fn random_tick_light(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32) -> u8 {
    let block = random_tick_block_light(reg, dim, x, y, z);
    let sky = if random_tick_sky_exposed(dim, x, y, z) {
        15
    } else {
        0
    };
    block.max(sky)
}

/// `Biome.shouldSnow`/`Biome.coldEnoughToSnow` from the biome the generator
/// resolves at the cell. Cold means the biome's `temperature` is below 0.15.
fn cold_biome_at(dim: DimId, x: i32, y: i32, z: i32) -> bool {
    if dim == DimId::Nether || dim == DimId::End {
        return false;
    }
    worldgen::with_gen_dim(dim, |g| {
        g.biome_blended(x, y, z)
            .is_some_and(|(_, leaf)| g.biome_cold_at_leaf(leaf, x, y, z))
    })
    .unwrap_or(false)
}

/// Vanilla grows sugar cane one stage per random tick, from age 0 to 15, and the tick
/// that would take it past 15 places a new cane above and resets the age. Sixteen
/// random ticks therefore add one block to the column, up to a height of three.
fn sugar_cane_random_tick(
    reg: &crate::block_registry::BReg,
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
) {
    if !reg.is_air(block_at_dim(dim, x, y + 1, z)) {
        return;
    }
    let mut height = 1;
    while reg.name_of(block_at_dim(dim, x, y - height, z)) == "minecraft:sugar_cane" {
        height += 1;
    }
    if height >= 3 {
        return;
    }
    let age: u32 = reg
        .get_prop(st, "age")
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);
    if age >= 15 {
        set_block_dim(dim, x, y + 1, z, reg.state_of("minecraft:sugar_cane", None));
        set_block_dim(dim, x, y, z, reg.with_prop(st, "age", "0"));
    } else {
        set_block_dim(dim, x, y, z, reg.with_prop(st, "age", &(age + 1).to_string()));
    }
}

/// Reads the world around a crop into `CropTickInput` and writes the evaluated
/// `age` back. Moisture and the 3x3 farmland context come from the cells the
/// crop actually neighbours; the same-crop diagonal rule samples the four
/// orthogonal neighbours.
fn crop_random_tick(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    let name = reg.name_of(st);
    let kind = match name {
        "minecraft:wheat" => CropKind::Wheat,
        "minecraft:carrots" => CropKind::Carrots,
        "minecraft:potatoes" => CropKind::Potatoes,
        "minecraft:beetroots" => CropKind::Beetroots,
        "minecraft:melon_stem" => CropKind::MelonStem,
        "minecraft:pumpkin_stem" => CropKind::PumpkinStem,
        "minecraft:sweet_berry_bush" => CropKind::SweetBerryBush,
        "minecraft:cocoa" => CropKind::Cocoa,
        _ => CropKind::NetherWart,
    };
    let age: u8 = reg
        .get_prop(st, "age")
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);
    let mut farmland_3x3 = [[None; 3]; 3];
    for dz in 0..3usize {
        for dx in 0..3usize {
            let b = block_at_dim(dim, x + dx as i32 - 1, y - 1, z + dz as i32 - 1);
            if reg.name_of(b) == "minecraft:farmland" {
                farmland_3x3[dz][dx] = reg
                    .get_prop(b, "moisture")
                    .and_then(|m| m.parse().ok());
            }
        }
    }
    let same_crop = |dx: i32, dz: i32| reg.name_of(block_at_dim(dim, x + dx, y, z + dz)) == name;
    let input = CropTickInput {
        kind,
        age,
        light: random_tick_light(reg, dim, x, y, z),
        farmland_below: farmland_3x3[1][1].is_some(),
        farmland_3x3,
        same_crop_north_south: same_crop(0, -1) || same_crop(0, 1),
        same_crop_east_west: same_crop(-1, 0) || same_crop(1, 0),
        random_value: world_rng::next_u64() as u32,
    };
    if let Some(new_age) = eval_crop_random_tick(input) {
        set_block_dim(dim, x, y, z, reg.with_prop(st, "age", &new_age.to_string()));
        note_block_update_dim(dim, x, y, z, st);
    }
}

/// The vanilla catch-fire cover: blocks `BaseFireBlock` lights on a random
/// tick. There is no flammable tag in the assets, so this names the common
/// combustible materials directly.
fn fire_flammable(reg: &crate::block_registry::BReg, st: u32) -> bool {
    if reg.in_tag(st, "minecraft:logs_that_burn") || reg.in_tag(st, "minecraft:leaves") {
        return true;
    }
    let name = reg.name_of(st);
    name.ends_with("_planks")
        || name.ends_with("_wool")
        || name.ends_with("_carpet")
        || name.ends_with("_log")
        || name.ends_with("_wood")
        || (name.ends_with("_fence") || name.ends_with("_fence_gate"))
            && !name.contains("nether_brick")
        || name.ends_with("_bookshelf")
        || matches!(
            name,
            "minecraft:tnt"
                | "minecraft:coal_block"
                | "minecraft:hay_block"
                | "minecraft:dried_kelp_block"
                | "minecraft:composter"
                | "minecraft:beehive"
                | "minecraft:bookshelf"
                | "minecraft:target"
                | "minecraft:bamboo"
        )
}

/// `FireBlock.tick` reduced to its observable outcomes: rain and the
/// `doFireTick` gamerule gate the whole pass; the evaluator owns the age,
/// burnout and spread probabilities.
fn fire_random_tick(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    if GR_DO_FIRE_TICK.load(Ordering::Relaxed) == 0 {
        return;
    }
    let soul_fire = reg.name_of(st) == "minecraft:soul_fire";
    let dirs = [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ];
    let mut flammable_neighbors = [false; 6];
    let mut spread_rolls = [0u8; 6];
    for i in 0..6 {
        let (dx, dy, dz) = dirs[i];
        flammable_neighbors[i] = fire_flammable(reg, block_at_dim(dim, x + dx, y + dy, z + dz));
        spread_rolls[i] = world_rng::next_below(16) as u8;
    }
    let age: u8 = reg
        .get_prop(st, "age")
        .and_then(|a| a.parse().ok())
        .unwrap_or(0);
    let result = eval_fire_tick(FireTickInput {
        age,
        rain_exposed: RAINING.load(Ordering::Relaxed) != 0
            && dim == DimId::Overworld
            && random_tick_sky_exposed(dim, x, y, z),
        soul_fire,
        flammable_neighbors,
        spread_rolls,
        age_roll: world_rng::next_below(256) as u8,
        burnout_roll: world_rng::next_below(256) as u8,
    });
    if result.extinguished || result.burned_out {
        set_block_dim(dim, x, y, z, 0);
        note_block_update_dim(dim, x, y, z, 0);
        return;
    }
    if let Some(new_age) = result.age {
        if new_age != age {
            set_block_dim(dim, x, y, z, reg.with_prop(st, "age", &new_age.to_string()));
            note_block_update_dim(dim, x, y, z, st);
        }
    }
    for i in 0..6 {
        if !result.spread[i] {
            continue;
        }
        let (dx, dy, dz) = dirs[i];
        let (nx, ny, nz) = (x + dx, y + dy, z + dz);
        if fire_flammable(reg, block_at_dim(dim, nx, ny, nz)) {
            let fire = reg.state_of("minecraft:fire", None);
            set_block_dim(dim, nx, ny, nz, fire);
            note_block_update_dim(dim, nx, ny, nz, fire);
        }
    }
}

/// `LeavesBlock.randomTick`: a leaf with `persistent=false` and `distance=7`
/// decays; one tick in twenty drops the block's loot. `doTileDrops` gates the
/// drop, not the decay.
fn leaf_random_tick(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    let persistent = reg.get_prop(st, "persistent").as_deref() == Some("true");
    let distance: u8 = reg
        .get_prop(st, "distance")
        .and_then(|d| d.parse().ok())
        .unwrap_or(1);
    let result = eval_leaf_decay(LeafDecayInput {
        persistent,
        distance,
        random_roll: world_rng::next_below(256) as u8,
    });
    if !result.decay {
        return;
    }
    set_block_dim(dim, x, y, z, 0);
    note_block_update_dim(dim, x, y, z, 0);
    if result.drop_item && GR_DO_TILE_DROPS.load(Ordering::Relaxed) != 0 {
        for item in block_drops(st) {
            drops().lock().unwrap().push(Drop {
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                x: x as f64 + 0.5,
                y: y as f64 + 0.4,
                z: z as f64 + 0.5,
                item,
                cnt: 1,
                comps: Vec::new(),
                age: 0,
                target: None,
                thrower: None,
            });
        }
    }
}

/// `IceBlock.randomTick`: block light of 11 or more melts the block — to a
/// water source where water can exist, to nothing in an ultrawarm dimension.
fn ice_random_tick(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32) {
    let input = SnowIceTickInput {
        cold_biome: cold_biome_at(dim, x, y, z),
        sky_exposed: random_tick_sky_exposed(dim, x, y, z),
        light: random_tick_block_light(reg, dim, x, y, z),
        support_is_full_solid: false,
        water_source: false,
        random_roll: world_rng::next_below(256) as u8,
    };
    if eval_ice_freeze_melt(input, true) == SnowIceTickResult::MeltIce
    {
        let out = if dim_ultrawarm(dim) {
            0
        } else {
            reg.state_of("minecraft:water", None)
        };
        set_block_dim(dim, x, y, z, out);
        note_block_update_dim(dim, x, y, z, out);
    }
}

/// `SnowAndFreezeCheck`/`LiquidBlock`: a water source in a biome cold enough to
/// snow freezes to ice when it can see the sky and its block light stays under
/// 11. Flowing water does not freeze.
fn water_random_tick(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    let source = fluid_from_block(st).is_some_and(|f| f.source());
    if !source {
        return;
    }
    let input = SnowIceTickInput {
        cold_biome: cold_biome_at(dim, x, y, z),
        sky_exposed: random_tick_sky_exposed(dim, x, y, z),
        light: random_tick_block_light(reg, dim, x, y, z),
        support_is_full_solid: false,
        water_source: true,
        random_roll: world_rng::next_below(256) as u8,
    };
    if eval_ice_freeze_melt(input, false)
        == SnowIceTickResult::FreezeWater
    {
        let ice = reg.state_of("minecraft:ice", None);
        set_block_dim(dim, x, y, z, ice);
        note_block_update_dim(dim, x, y, z, ice);
    }
}

/// `SnowLayerBlock.randomTick`: while it is snowing a layer may deepen, up to
/// the `snowAccumulationHeight` gamerule — and that depth still needs the same
/// cold, sky and low-light conditions water needs to freeze, plus a full solid
/// layer above to gather the fall. `0` disables accumulation entirely.
fn snow_random_tick(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    let cap = gr_snow_accumulation_height();
    if cap == 0 || RAINING.load(Ordering::Relaxed) == 0 || dim != DimId::Overworld {
        return;
    }
    let layers: u8 = reg
        .get_prop(st, "layers")
        .and_then(|l| l.parse().ok())
        .unwrap_or(1);
    if layers >= cap {
        return;
    }
    let input = SnowIceTickInput {
        cold_biome: cold_biome_at(dim, x, y, z),
        sky_exposed: random_tick_sky_exposed(dim, x, y, z),
        light: random_tick_block_light(reg, dim, x, y, z),
        support_is_full_solid: reg.is_collision_shape_full_block(block_at_dim(dim, x, y + 1, z)),
        water_source: false,
        random_roll: world_rng::next_below(256) as u8,
    };
    if eval_snow_accumulation(input)
        == SnowIceTickResult::AccumulateSnow
    {
        let next = reg.with_prop(st, "layers", &(layers + 1).to_string());
        set_block_dim(dim, x, y, z, next);
        note_block_update_dim(dim, x, y, z, next);
    }
}

/// `SaplingBlock.randomTick`/`BonemealableFeaturePlaceSection`: a stage-0
/// sapling under sky-equivalent light advances to stage 1; a stage-1 sapling
/// with the headroom grows a tree. The roll matches vanilla's `nextInt(7)`.
fn sapling_random_tick(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    let stage: u8 = reg
        .get_prop(st, "stage")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    // The canopy needs y+2..y+5 clear of everything but air and the sapling
    // itself before a grow is offered to the evaluator.
    let headroom = (2..=5).all(|dy| reg.is_air(block_at_dim(dim, x, y + dy, z)));
    let input = SaplingTickInput {
        stage,
        light: random_tick_light(reg, dim, x, y + 1, z),
        headroom,
        random_roll: world_rng::next_below(64) as u8,
    };
    match eval_sapling_random_tick(input) {
        SaplingTickResult::NoChange => {}
        SaplingTickResult::AdvanceStage => {
            let next = reg.with_prop(st, "stage", "1");
            set_block_dim(dim, x, y, z, next);
            note_block_update_dim(dim, x, y, z, next);
        }
        SaplingTickResult::Grow => {
            grow_sapling_tree(reg, dim, x, y, z, reg.name_of(st));
        }
    }
}

/// The trunk/leaf pair a `*_sapling` name resolves to. Saplings without a
/// canopy pair — `mangrove_propagule` is not a `*_sapling` — never reach this.
fn sapling_tree_blocks(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "minecraft:oak_sapling" => ("minecraft:oak_log", "minecraft:oak_leaves"),
        "minecraft:birch_sapling" => ("minecraft:birch_log", "minecraft:birch_leaves"),
        "minecraft:spruce_sapling" => ("minecraft:spruce_log", "minecraft:spruce_leaves"),
        "minecraft:jungle_sapling" => ("minecraft:jungle_log", "minecraft:jungle_leaves"),
        "minecraft:acacia_sapling" => ("minecraft:acacia_log", "minecraft:acacia_leaves"),
        "minecraft:dark_oak_sapling" => ("minecraft:dark_oak_log", "minecraft:dark_oak_leaves"),
        "minecraft:cherry_sapling" => ("minecraft:cherry_log", "minecraft:cherry_leaves"),
        _ => return None,
    })
}

/// The smallest structurally-valid tree: a four-block trunk, a 3x3 leaf ring at
/// y+2 and y+3, and a plus of leaves at y+4. Leaves are placed
/// `persistent=false, distance=1` so they behave like generated canopy. Only
/// air and the sapling cell are replaced.
fn grow_sapling_tree(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32, name: &str) {
    let Some((log_name, leaf_name)) = sapling_tree_blocks(name) else {
        return;
    };
    let log = reg.state_of(log_name, None);
    let mut leaves = reg.state_of(leaf_name, None);
    if reg.get_prop(leaves, "persistent").is_some() {
        leaves = reg.with_prop(leaves, "persistent", "false");
    }
    if reg.get_prop(leaves, "distance").is_some() {
        leaves = reg.with_prop(leaves, "distance", "1");
    }
    let set = |bx: i32, by: i32, bz: i32, st: u32| {
        let cur = block_at_dim(dim, bx, by, bz);
        if reg.is_air(cur) || cur == st || (by == y && bx == x && bz == z) {
            set_block_dim(dim, bx, by, bz, st);
            note_block_update_dim(dim, bx, by, bz, st);
        }
    };
    for dy in 0..4 {
        set(x, y + dy, z, log);
    }
    for dy in [2, 3] {
        for dx in -1..=1 {
            for dz in -1..=1 {
                if dx != 0 || dz != 0 {
                    set(x + dx, y + dy, z + dz, leaves);
                }
            }
        }
    }
    for (dx, dz) in [(0, -1), (-1, 0), (1, 0), (0, 1)] {
        set(x + dx, y + 4, z + dz, leaves);
    }
}

/// `SpreadingSnowyDirtBlock` (`GrassBlock`/`MyceliumBlock`): an opaque block
/// overhead with light below 4 kills the grass back to dirt; otherwise a lit
/// grass block tries four random offsets and converts a dirt cell whose top is
/// clear. The `snowy` flag follows a snow cover above the target.
fn grass_random_tick(reg: &crate::block_registry::BReg, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    let name = reg.name_of(st);
    let above = block_at_dim(dim, x, y + 1, z);
    let covered_opaque = reg.can_occlude(above);
    let light_above = random_tick_light(reg, dim, x, y + 1, z);
    let input = |target_is_dirt, target_above_blocks| GrassSpreadInput {
        covered_opaque,
        light_above,
        target_is_dirt,
        target_above_blocks,
    };
    if eval_grass_spread(input(false, false)) == GrassSpreadResult::DieToDirt {
        let dead = reg.state_of("minecraft:dirt", None);
        set_block_dim(dim, x, y, z, dead);
        note_block_update_dim(dim, x, y, z, dead);
        return;
    }
    for _ in 0..4 {
        let nx = x + world_rng::next_below(3) as i32 - 1;
        let ny = y + world_rng::next_below(5) as i32 - 3;
        let nz = z + world_rng::next_below(3) as i32 - 1;
        let target_above = block_at_dim(dim, nx, ny + 1, nz);
        let target_is_dirt =
            reg.name_of(block_at_dim(dim, nx, ny, nz)) == "minecraft:dirt";
        let target_above_blocks =
            reg.can_occlude(target_above) || reg.is_liquid(target_above);
        if eval_grass_spread(input(target_is_dirt, target_above_blocks))
            != GrassSpreadResult::SpreadToNeighbor
        {
            continue;
        }
        let mut grass = reg.state_of(name, None);
        if reg.get_prop(grass, "snowy").is_some() {
            let snowy = reg.name_of(target_above) == "minecraft:snow_block";
            grass = reg.with_prop(grass, "snowy", if snowy { "true" } else { "false" });
        }
        set_block_dim(dim, nx, ny, nz, grass);
        note_block_update_dim(dim, nx, ny, nz, grass);
        return;
    }
}

fn sched_invalidate(x: i32, y: i32, z: i32) {
    // The cell and its six neighbours are woken. Clearing the backoff alone is not
    // enough: the delay queues hold their own deadline, so a cell that is already
    // waiting would keep its old slot and not be visited until it came round,
    // which is the opposite of waking it. Promote it to the next tick as well.
    let cells: [(i32, i32, i32); 7] = [
        (x, y, z),
        (x + 1, y, z),
        (x - 1, y, z),
        (x, y + 1, z),
        (x, y - 1, z),
        (x, y, z + 1),
        (x, y, z - 1),
    ];
    {
        let mut b = backoff().lock().unwrap();
        for c in cells {
            b.remove(&(DimId::Overworld, c.0, c.1, c.2));
        }
    }
    if !water_queue().lock().unwrap().is_empty() {
        let mut q = water_queue().lock().unwrap();
        for c in cells {
            q.promote(c);
        }
    }
    if !lava_queue().lock().unwrap().is_empty() {
        let mut q = lava_queue().lock().unwrap();
        for c in cells {
            q.promote(c);
        }
    }
    if !hopper_queue().lock().unwrap().is_empty() {
        let mut q = hopper_queue().lock().unwrap();
        for c in cells {
            q.promote(c);
        }
    }
}

// ---------- native plugin trampoline (contract v0) ----------
// Plugins never get a raw tick callback. The only execution path is
// schedule_cell -> host calls tick_cell when (a) region tier allows,
// (b) backoff elapsed or invalidated, (c) fuel remains. A plugin cannot
// reintroduce global polling: cold cells defer, no-op cells back off,
// and every block_write flows through set_block (occlusion + batch +
// invalidation) exactly like internal systems.
#[derive(Clone, Copy, PartialEq)]
enum WorkResult {
    Work,
    NoOp,
}

const PLUGIN_FUEL: i32 = 256; // host calls per tick_cell
const PLUGIN_MAX_CELLS: usize = 4096; // per-plugin scheduled volume cap

pub struct PluginApi {
    fuel: i32,
    pi: usize,
}
impl PluginApi {
    fn spend(&mut self) -> Result<(), &'static str> {
        if self.fuel <= 0 {
            return Err("fuel");
        }
        self.fuel -= 1;
        Ok(())
    }
    pub fn block_get(&mut self, x: i32, y: i32, z: i32) -> Result<u32, &'static str> {
        self.spend()?;
        Ok(block_at(x, y, z))
    }
    pub fn block_set(&mut self, x: i32, y: i32, z: i32, st: u32) -> Result<(), &'static str> {
        self.spend()?;
        set_block(x, y, z, st);
        Ok(())
    }
    /// Declare a state id hidden (ore/vault/etc): occlusion masking and
    /// reveal-on-expose apply to it exactly like the built-in set.
    pub fn mark_hidden(&mut self, st: u32) -> Result<(), &'static str> {
        self.spend()?;
        hidden_extra().lock().unwrap().insert(st);
        HIDDEN_EXTRA_USED.store(true, Ordering::Relaxed);
        Ok(())
    }
    pub fn schedule_cell(
        &mut self,
        x: i32,
        y: i32,
        z: i32,
        key: u32,
        warm_ok: bool,
    ) -> Result<(), &'static str> {
        self.spend()?;
        let mut cells = plugin_cells().lock().unwrap();
        if cells.values().flatten().filter(|c| c.pi == self.pi).count() >= PLUGIN_MAX_CELLS {
            return Err("cell cap");
        }
        let bucket = cells.entry((DimId::Overworld, x >> 4, z >> 4)).or_default();
        if !bucket
            .iter()
            .any(|c| c.pi == self.pi && c.x == x && c.y == y && c.z == z && c.key == key)
        {
            bucket.push(PluginCell {
                pi: self.pi,
                x,
                y,
                z,
                key,
                warm_ok,
            });
        }
        Ok(())
    }
}

trait Plugin: Send {
    fn id(&self) -> &'static str;
    // called only when cell eligible + region tier allows
    fn tick_cell(&mut self, api: &mut PluginApi, x: i32, y: i32, z: i32, key: u32) -> WorkResult;
}

#[derive(Clone)]
struct PluginCell {
    pi: usize,
    x: i32,
    y: i32,
    z: i32,
    key: u32,
    warm_ok: bool,
}
type CellKey = (DimId, usize, i32, i32, i32, u32);
static PLUGIN_CELLS: Registry<DimChunkKey, Vec<PluginCell>> = OnceLock::new();
static PLUGIN_BACKOFF: OnceLock<Mutex<HashMap<CellKey, (u64, u8)>>> = OnceLock::new();
static PLUGINS: OnceLock<Mutex<Vec<Box<dyn Plugin>>>> = OnceLock::new();
fn plugin_cells() -> &'static Mutex<HashMap<DimChunkKey, Vec<PluginCell>>> {
    PLUGIN_CELLS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn plugin_backoff() -> &'static Mutex<HashMap<CellKey, (u64, u8)>> {
    PLUGIN_BACKOFF.get_or_init(|| Mutex::new(HashMap::new()))
}
fn plugins() -> &'static Mutex<Vec<Box<dyn Plugin>>> {
    PLUGINS.get_or_init(|| {
        let mut v: Vec<Box<dyn Plugin>> = Vec::new();
        let want = std::env::var("MC_SPINE_PLUGINS").unwrap_or_default();
        if want.split(',').any(|w| w.trim() == "growthlamp") {
            v.push(Box::new(GrowthLamp));
        }
        if !v.is_empty() {
            eprintln!(
                "[plugins] loaded: {}",
                v.iter().map(|p| p.id()).collect::<Vec<_>>().join(",")
            );
        }
        Mutex::new(v)
    })
}

fn plugin_sched_remove(x: i32, y: i32, z: i32) {
    let mut cells = plugin_cells().lock().unwrap();
    let ck = (DimId::Overworld, x >> 4, z >> 4);
    if let Some(b) = cells.get_mut(&ck) {
        b.retain(|c| !(c.x == x && c.y == y && c.z == z));
        if b.is_empty() {
            cells.remove(&ck);
        }
    }
}

// every set_block wakes plugin cells within Chebyshev-1 (vanilla
// neighbor-update reach) — invalidation is NOT depth-gated.
fn plugin_invalidate(x: i32, y: i32, z: i32) {
    let cells = plugin_cells().lock().unwrap();
    if cells.is_empty() {
        return;
    }
    // a cell within Chebyshev-1 lives in one of the 3x3 surrounding chunks
    let mut b = plugin_backoff().lock().unwrap();
    for cx in (x >> 4) - 1..=(x >> 4) + 1 {
        for cz in (z >> 4) - 1..=(z >> 4) + 1 {
            if let Some(bucket) = cells.get(&(DimId::Overworld, cx, cz)) {
                for c in bucket {
                    if (c.x - x).abs() <= 1 && (c.y - y).abs() <= 1 && (c.z - z).abs() <= 1 {
                        b.remove(&(DimId::Overworld, c.pi, c.x, c.y, c.z, c.key));
                    }
                }
            }
        }
    }
}

fn plugin_tick() {
    let nplug = { plugins().lock().unwrap().len() };
    if nplug == 0 {
        return;
    }
    let cells: Vec<PluginCell> = plugin_cells()
        .lock()
        .unwrap()
        .values()
        .flatten()
        .cloned()
        .collect();
    if cells.is_empty() {
        return;
    }
    let region = region_guard();
    let now = TICK_IDX.load(Ordering::Relaxed);
    for c in cells {
        let ck = (DimId::Overworld, c.pi, c.x, c.y, c.z, c.key);
        let due = plugin_backoff()
            .lock()
            .unwrap()
            .get(&ck)
            .map(|(t, _)| *t <= now)
            .unwrap_or(true);
        if !due {
            continue;
        }
        if !(region.is_hot(c.x, c.z) || (c.warm_ok && region.is_warm(c.x, c.z))) {
            continue;
        }
        let mut api = PluginApi {
            fuel: PLUGIN_FUEL,
            pi: c.pi,
        };
        let res = {
            let mut pl = plugins().lock().unwrap();
            match pl.get_mut(c.pi) {
                Some(p) => p.tick_cell(&mut api, c.x, c.y, c.z, c.key),
                None => continue,
            }
        };
        let mut b = plugin_backoff().lock().unwrap();
        if res == WorkResult::Work {
            b.remove(&ck);
        } else {
            let f = b.get(&ck).map(|e| e.1).unwrap_or(0) + 1;
            b.insert(ck, (now + (1u64 << f.min(4)), f.min(4)));
        }
    }
}

// demo plugin: growth lamp (custom state 9900) — a Create-analogue
// machine that does work on nearby blocks. Placing 9900 registers a
// cell; the cell ticks only in hot/warm regions, backs off 4s when no
// crops are near, and wakes instantly when a crop is planted.
const ST_GROWTH_LAMP: u32 = 9900;
struct GrowthLamp;
impl Plugin for GrowthLamp {
    fn id(&self) -> &'static str {
        "growthlamp"
    }
    fn tick_cell(&mut self, api: &mut PluginApi, x: i32, y: i32, z: i32, _key: u32) -> WorkResult {
        let mut did = false;
        for dx in -2..=2 {
            for dy in -2..=2 {
                for dz in -2..=2 {
                    let (bx, by, bz) = (x + dx, y + dy, z + dz);
                    match api.block_get(bx, by, bz) {
                        Ok(st)
                            if (4278..4285).contains(&st)
                                && api.block_set(bx, by, bz, st + 1).is_ok() =>
                        {
                            did = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        if did {
            WorkResult::Work
        } else {
            WorkResult::NoOp
        }
    }
}

// per-tick block-change batching (vanilla multi_block_change, 0x49):
// N individual block_change packets -> 1 packet per touched section.
static PENDING_BLOCKS: Registry<DimBlockPos, HashMap<u16, u32>> = OnceLock::new();
fn pending_blocks() -> &'static LockedMap<DimBlockPos, HashMap<u16, u32>> {
    PENDING_BLOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}
fn note_block_update(x: i32, y: i32, z: i32, st: u32) {
    note_block_update_dim(DimId::Overworld, x, y, z, st)
}
fn visible_block_update_state(dim: DimId, x: i32, y: i32, z: i32, st: u32) -> u32 {
    // A hidden, unexposed block's true ID cannot reach the wire.
    if is_hidden(st) && !block_exposed_dim(dim, x, y, z) {
        hidden_filler_dim(dim, x, y, z, filler_height(dim, x, z))
    } else {
        st
    }
}
fn note_block_update_dim(dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    let out = visible_block_update_state(dim, x, y, z, st);
    pending_blocks()
        .lock()
        .unwrap()
        .entry((
            dim,
            x.div_euclid(16),
            y.div_euclid(16),
            z.div_euclid(16),
        ))
        .or_default()
        .insert((((x & 15) << 8) | ((z & 15) << 4) | (y & 15)) as u16, out);
}
fn apply_queued_block_updates() {
    // The tick and connection threads can both flush. Publish batches in order.
    static FLUSH: Mutex<()> = Mutex::new(());
    let _flush = FLUSH.lock().unwrap();
    let map = std::mem::take(&mut *pending_blocks().lock().unwrap());
    for ((dim, sx, sy, sz), recs) in map {
        let mut p = Vec::with_capacity(9 + recs.len() * 5);
        let key =
            ((sx as i64 & 0x3fffff) << 42) | ((sz as i64 & 0x3fffff) << 20) | (sy as i64 & 0xfffff);
        p.extend_from_slice(&key.to_be_bytes());
        write_varint(&mut p, recs.len() as i64);
        for (idx, st) in &recs {
            write_varint(&mut p, ((*st as i64) << 12) | *idx as i64);
        }
        broadcast_chunk_dim(dim, sx, sz, |_c| (0x49, p.clone()));
    }
    apply_queued_light_updates();
}

fn local_idx(x: i32, y: i32, z: i32) -> u32 {
    (((y - MIN_Y) as u32) << 8) | (((z & 15) as u32) << 4) | (x & 15) as u32
}
/// The overlay's state for a cell in one dimension, or `None` when the cell still holds that
/// dimension's generated terrain. Use this instead of `block_at` where the *only* states that
/// matter are ones a player or a tick wrote: `block_at` falls back to `base_state` → a full locked
/// worldgen evaluation (density + material rule + aquifer + feature population) for
/// every generated cell, measured at 1.6 ms per call in the pressure-plate loop —
/// 19.5 ms of a tick, for 12 entity positions, none of which was on a plate.
fn overlay_block_dim(dim: DimId, x: i32, y: i32, z: i32) -> Option<u32> {
    world()
        .lock()
        .unwrap()
        .get(&(dim, x >> 4, z >> 4))
        .and_then(|c| c.get(&local_idx(x, y, z)).copied())
}

fn overlay_block(x: i32, y: i32, z: i32) -> Option<u32> {
    overlay_block_dim(DimId::Overworld, x, y, z)
}

/// The generated state for a cell with no edit, from the generator that owns `dim`.
///
/// The Overworld arm is `base_state`, which is what every existing reader measured. The other two
/// ask *their own* generator through the per-dimension registry, and answer air when that
/// dimension has no generator installed. They must not fall back to `base_state`: that resolves
/// the Overworld's noise settings and its -64/384 window, so a Nether cell below y=0 would come
/// back as Overworld deepslate instead of the air the Nether's 0/256 window defines — the exact
/// substitution the per-dimension registry exists to stop.
fn dim_base_state(dim: DimId, x: i32, y: i32, z: i32) -> Option<u32> {
    if dim == DimId::Overworld {
        return Some(base_state(x, y, z));
    }
    worldgen::custom_dim_state(dim, x, y, z).or_else(|| {
        worldgen::with_gen_dim(dim, |g| g.block_state_at(x, y, z, crate::feature::populate))
    })
}

/// The generated state at a cell in `dim`, air when that dimension has no generator. The
/// Overworld arm is exactly `base_state`, so the existing Overworld packet bytes are moved by
/// neither the name nor the fallback here.
fn base_state_dim(dim: DimId, x: i32, y: i32, z: i32) -> u32 {
    dim_base_state(dim, x, y, z).unwrap_or(0)
}

fn block_at_dim(dim: DimId, x: i32, y: i32, z: i32) -> u32 {
    let w = timed_world();
    let hit = w
        .get(&(dim, x >> 4, z >> 4))
        .and_then(|c| c.get(&local_idx(x, y, z)).copied());
    if chunkprof::enabled() {
        use std::sync::atomic::Ordering;
        chunkprof::BLOCK_AT.fetch_add(1, Ordering::Relaxed);
        if hit.is_none() {
            chunkprof::BLOCK_AT_MISS.fetch_add(1, Ordering::Relaxed);
        }
    }
    drop(w); // Never acquire SAVE_LOCK through generation while holding WORLD.
    hit.unwrap_or_else(|| dim_base_state(dim, x, y, z).unwrap_or(0))
}

fn block_at(x: i32, y: i32, z: i32) -> u32 {
    block_at_dim(DimId::Overworld, x, y, z)
}
fn set_block_dim(dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    set_block_if_dim(dim, x, y, z, st, None);
}
fn set_block(x: i32, y: i32, z: i32, st: u32) {
    set_block_dim(DimId::Overworld, x, y, z, st);
}
/// A block write and the scheduled-tick reservations it causes are one
/// persistence transaction. The signal-thread save holds `SAVE_LOCK` while it
/// serialises the world and the queue, so a snapshot taken between the write
/// and `block_updated`'s booking would persist a gravity block with no
/// callback and leave it hanging after a reload. Re-entrant, so a nested
/// write (a cascade placing another block) does not take the lock twice.
fn set_block_if_dim(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
    expected: Option<u32>,
) -> bool {
    with_save_guard(|| set_block_if_inner_dim(dim, x, y, z, st, expected))
}
fn set_block_if(x: i32, y: i32, z: i32, st: u32, expected: Option<u32>) -> bool {
    set_block_if_dim(DimId::Overworld, x, y, z, st, expected)
}

/// Apply a block edit and notify the subsystems for its dimension.
///
/// The Overworld additionally owns the legacy position-only registries for
/// scheduled blocks, fluids, hoppers, crops, spawners, and chest pairing.
fn set_block_if_inner_dim(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
    expected: Option<u32>,
) -> bool {
    if dim != DimId::Overworld {
        return set_block_if_dim_edit(dim, x, y, z, st, expected);
    }
    let mut w = world().lock().unwrap();
    let c = w.entry((dim, x >> 4, z >> 4)).or_default();
    let old = c
        .get(&local_idx(x, y, z))
        .copied()
        .unwrap_or_else(|| base_state(x, y, z));
    if expected.is_some_and(|value| old != value) {
        return false;
    }
    if st == base_state(x, y, z) {
        c.remove(&local_idx(x, y, z));
    } else {
        c.insert(local_idx(x, y, z), st);
    }
    drop(w);
    if Some(st) != name_to_state("spawner") {
        generated_content().lock().unwrap().spawners.remove(&(x,y,z));
    }
    // The band is a write-through view of the chunk, not an invalidated one: a leaf tick
    // writes every tick, so invalidating would rebuild a whole band per write (14 ms).
    // The written cell is patched instead, which keeps the band equal to `block_at` for
    // every cell it covers. The version still moves for the sky floors, which are derived
    // from the column rather than stored per cell.
    band_patch_dim(dim, x, y, z, st);
    // Only Overworld reaches this point (the early return above handles the other
    // dimensions), but pass `dim` so the patched store slot keeps its dimension key.
    store_patch(dim, x, y, z, st);
    bump_world_epoch_dim(dim, x, z);
    note_light_edit(dim, Position::new(x, y, z), old, st);
    let native_fluid_controlled = generated_fluid_changed((x, y, z), old, st);
    if st != old {
        schedule_leaf_neighbors(x, y, z, st);
        // Identity, not state: `Containers.dropContentsOnDestroy` is guarded by
        // `!state.is(newState.getBlock())`, so a chest that is waterlogged, re-faced or
        // re-paired keeps its contents. Guarding this on `st != old` made every state
        // write on the same block throw the container away.
        let identity_changed = match breg() {
            Some(reg) => reg.name_of(old) != reg.name_of(st),
            None => true,
        };
        if identity_changed {
            // per-position metadata cannot survive the block that owned it (a new
            // chest at a broken chest's spot must NOT resurrect the old contents —
            // that is an item-dupe).
            signs()
                .lock()
                .unwrap()
                .retain(|&(dim, sx, sy, sz, _), _| {
                    !(dim == DimId::Overworld && sx == x && sy == y && sz == z)
                });
            // The halves this container was part of, read from the state it still had
            // as a block: the pair lives in the block state, so the *old* state is the
            // one that knows it.
            let halves = chest_halves_of(old, (x, y, z));
            // broken containers throw their contents (vanilla): one drop
            // entity per stack, not per item.
            unpack_block_loot_if_needed(ow_block((x, y, z)));
            let mut spill: Vec<Stack> = Vec::new();
            if let Some(slots) = chests()
                .lock()
                .unwrap()
                .remove(&ContainerKey::ow((x, y, z)))
            {
                spill.extend(
                    slots
                        .values()
                        .filter(|&(it, cn, _)| *it > 0 && *cn > 0)
                        .cloned(),
                );
            }
            if let Some(f) = furnaces().lock().unwrap().remove(&ow_block((x, y, z))) {
                spill.extend(
                    [f.input.clone(), f.fuel.clone(), f.output.clone()]
                        .into_iter()
                        .filter(|(it, cn, _)| *it > 0 && *cn > 0),
                );
            }
            if !spill.is_empty()
                && GR_DO_TILE_DROPS.load(Ordering::Relaxed) != 0
                && w1_explosion_drop_allowed()
            {
                let mut ds = drops().lock().unwrap();
                for (item, cnt, comps) in spill {
                    ds.push(Drop {
                        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                        x: x as f64 + 0.5,
                        y: y as f64 + 0.5,
                        z: z as f64 + 0.5,
                        item,
                        cnt,
                        comps,
                        age: 0,
                        target: None,
                        thrower: None,
                    });
                }
            }
            // `ChestBlockEntity.setRemoved` -> `recheckOpeners`: a menu built on a
            // container the world no longer has is closed, and the opener counters of
            // the halves it counted drop (one BlockEvent per half that moved).
            let audience = client_snapshot();
            invalidate_container_windows(
                &audience,
                ContainerKey::ow((x, y, z)),
                &halves,
            );
            if (7513..=7520).contains(&old) {
                invalidate_ender_chest_windows(&audience, (x, y, z));
            }
            if !(4278..=4285).contains(&st) {
                crops().lock().unwrap().remove(&ow_block((x, y, z)));
            }
        }
        // Chest pairing is block state, so the chests beside this cell re-derive it
        // (`ChestBlock.updateShape`). A repair writes the same block, which is why it
        // has to run outside the identity branch above: it must not touch contents.
        if chest_touches((x, y, z), old, st) {
            repair_chest_neighbours((x, y, z));
        }
    }
    let mut hh = hoppers().lock().unwrap();
    if (9225..=9234).contains(&st) {
        if hh.len() < HOPPER_CAP {
            hh.insert((x, y, z));
        }
        hopper_queue().lock().unwrap().schedule((x, y, z), 1);
    } else {
        hh.remove(&(x, y, z));
    }
    drop(hh);
    let mut wtr = waters().lock().unwrap();
    if (80..=95).contains(&st) {
        wtr.insert((x, y, z));
        if !native_fluid_controlled {
            water_queue().lock().unwrap().schedule((x, y, z), 1);
        }
    } else {
        wtr.remove(&(x, y, z));
    }
    drop(wtr);
    let mut lv = lavas().lock().unwrap();
    if (96..=111).contains(&st) {
        lv.insert((x, y, z));
        if !native_fluid_controlled {
            lava_queue().lock().unwrap().schedule((x, y, z), 1);
        }
    } else {
        lv.remove(&(x, y, z));
    }
    drop(lv);
    plugin_sched_remove(x, y, z);
    if st == ST_GROWTH_LAMP {
        plugin_cells()
            .lock()
            .unwrap()
            .entry((DimId::Overworld, x >> 4, z >> 4))
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
    refresh_hidden_neighbors(x, y, z);
    block_updated(x, y, z);
    live_block_changed(dim, RsPos(x, y, z), old, st);
    dwal_note_block_set(dim, x, y, z, st);
    true
}

/// A non-Overworld block write updates its overlay, caches, and dimension-aware
/// mechanisms. The overlay wins, and writing the generated state erases its edit.
fn set_block_if_dim_edit(
    dim: DimId,
    x: i32,
    y: i32,
    z: i32,
    st: u32,
    expected: Option<u32>,
) -> bool {
    // The generator's answer for this cell does not depend on the overlay, so it is taken before
    // the world lock: the write is the only thing this path serialises, and it is asked once.
    let base = dim_base_state(dim, x, y, z).unwrap_or(0);
    let cell = local_idx(x, y, z);
    let mut w = world().lock().unwrap();
    let key: DimChunkKey = (dim, x >> 4, z >> 4);
    let c = w.entry(key).or_default();
    let old = c.get(&cell).copied().unwrap_or(base);
    if expected.is_some_and(|value| old != value) {
        return false;
    }
    if st == base {
        c.remove(&cell);
    } else {
        c.insert(cell, st);
    }
    drop(w);
    // The non-Overworld write path used to stop at the overlay, leaving the band and the
    // section store answering with the *generated* state after an Nether/End edit. Those
    // caches are dimension-keyed now, so patching the dimension that was written is as
    // write-through here as it is on the Overworld path — with no chance of touching an
    // Overworld slot at the same `(cx, cz)`.
    band_patch_dim(dim, x, y, z, st);
    store_patch(dim, x, y, z, st);
    bump_world_epoch_dim(dim, x, z);
    note_light_edit(dim, Position::new(x, y, z), old, st);
    if old != st {
        // The Overworld path does this in `set_block_if_inner_dim`: a mined neighbour
        // reveals a hidden block, a filled one hides it again.
        refresh_hidden_neighbors_dim(dim, x, y, z);
    }
    live_block_changed(dim, RsPos(x, y, z), old, st);
    if old != st {
        if let Some(reg) = breg() {
            // The Overworld path retires a rewritten block's live block entity inside
            // `unpack_block_loot_if_needed` and drops sign text on identity change;
            // this path owns every other dimension, so the same rules have to run
            // here. Entries die with the block identity, not the state: a
            // same-block property write keeps them, a different block drops them.
            let mut store = live_block_entities().lock().unwrap();
            if let Some(&(id, _)) = store.entries.get(&(dim, x, y, z)) {
                if reg.block_id_of(st) != Some(id) {
                    store.entries.remove(&(dim, x, y, z));
                }
            }
            drop(store);
            if reg.name_of(old) != reg.name_of(st) {
                signs()
                    .lock()
                    .unwrap()
                    .retain(|&(sdim, sx, sy, sz, _), _| {
                        !(sdim == dim && sx == x && sy == y && sz == z)
                    });
            }
        }
    }
    dwal_note_block_set(dim, x, y, z, st);
    true
}

type LiveRedstoneTick = Reverse<(u64, RsTickPriority, u64, RsPos, u32)>;

#[derive(Default)]
struct LiveRedstoneState {
    comparator_outputs: BTreeMap<RsPos, u8>,
    torches: BTreeMap<RsPos, RsTorchHistory>,
    ticks: BinaryHeap<LiveRedstoneTick>,
    pending: BTreeMap<(RsPos, u32), u64>,
    sequence: u64,
}

fn live_redstone() -> &'static LockedMap<DimId, LiveRedstoneState> {
    static STATE: Registry<DimId, LiveRedstoneState> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

struct LiveRedstoneWorld<'a> {
    dim: DimId,
    reg: &'static BReg,
    state: &'a mut LiveRedstoneState,
    now: u64,
    reads: std::cell::RefCell<BTreeMap<RsPos, u32>>,
    writes: BTreeMap<RsPos, u32>,
    dispensers: Vec<RsPos>,
}

impl LiveRedstoneWorld<'_> {
    fn schedule_component(&mut self, pos: RsPos, delay: u64) {
        if let Some(block_id) = self.reg.block_id_of(self.state_at(pos)) {
            self.schedule_redstone_tick(RsWorldTick {
                pos,
                block_id: block_id as u32,
                delay,
                priority: RsTickPriority::Normal,
            });
        }
    }

    fn run_host_tick(&mut self, pos: RsPos, block_id: u32) {
        let state = self.state_at(pos);
        if self.reg.block_id_of(state).map(|id| id as u32) != Some(block_id) {
            return;
        }
        // These two devices have externally triggered edges. The shared device
        // scheduler handles their downstream effects once the host flips POWERED.
        match rs_block_from_state(self.reg, state) {
            RsBlock::Observer { powered, .. } => {
                let next =
                    self.reg
                        .with_prop(state, "powered", if powered { "false" } else { "true" });
                self.apply_redstone_change(pos, next, None);
                if !powered {
                    self.schedule_component(pos, 2);
                }
            }
            RsBlock::Button { powered: true, .. } => {
                self.apply_redstone_change(
                    pos,
                    self.reg.with_prop(state, "powered", "false"),
                    None,
                );
            }
            _ => {}
        }
    }
}

impl RedstoneBlockWorld for LiveRedstoneWorld<'_> {
    fn state_at(&self, pos @ RsPos(x, y, z): RsPos) -> u32 {
        if let Some(&state) = self.writes.get(&pos) {
            return state;
        }
        *self.reads.borrow_mut().entry(pos).or_insert_with(|| {
            band_state_dim(self.dim, x, y, z).unwrap_or_else(|| block_at_dim(self.dim, x, y, z))
        })
    }

    fn is_redstone_conductor(&self, pos: RsPos) -> bool {
        self.reg.is_redstone_conductor(self.state_at(pos))
    }

    fn comparator_output(&self, pos: RsPos) -> u8 {
        self.state
            .comparator_outputs
            .get(&pos)
            .copied()
            .unwrap_or(0)
    }

    fn analog_signal(&self, pos @ RsPos(x, y, z): RsPos) -> Option<u8> {
        // The positional read still records into `reads` like every other
        // accessor on this world view. The answer itself delegates to the one
        // live comparator reader: the W-BE store and the materialized block
        // entities (lectern, chiseled bookshelf, jukebox, decorated pot,
        // crafter, brewing stand, spawner) first, then the classic container
        // fullness for chests, barrels, dispensers, droppers, hoppers and
        // furnaces, in whichever dimension this pass runs in.
        self.state_at(pos);
        live_comparator_signal_at(self.dim, x, y, z)
    }

    fn apply_redstone_change(&mut self, pos: RsPos, state: u32, comparator_output: Option<u8>) {
        if let Some(output) = comparator_output {
            self.state.comparator_outputs.insert(pos, output);
        }
        if self.state_at(pos) != state {
            self.writes.insert(pos, state);
        }
    }

    fn schedule_redstone_tick(&mut self, tick: RsWorldTick) {
        let key = (tick.pos, tick.block_id);
        if let std::collections::btree_map::Entry::Vacant(entry) = self.state.pending.entry(key) {
            let sequence = self.state.sequence;
            self.state.sequence += 1;
            entry.insert(sequence);
            self.state.ticks.push(Reverse((
                self.now + server_ticks(tick.delay),
                tick.priority,
                sequence,
                tick.pos,
                tick.block_id,
            )));
        }
    }

    fn dispense_redstone(&mut self, pos: RsPos) {
        self.dispensers.push(pos);
    }
}

fn redstone_component(reg: &BReg, state: u32) -> bool {
    matches!(
        reg.name_of(state),
        "minecraft:redstone_wire"
            | "minecraft:redstone_block"
            | "minecraft:redstone_torch"
            | "minecraft:redstone_wall_torch"
            | "minecraft:lever"
            | "minecraft:repeater"
            | "minecraft:comparator"
            | "minecraft:redstone_lamp"
            | "minecraft:observer"
            | "minecraft:target"
            | "minecraft:hopper"
            | "minecraft:dispenser"
            | "minecraft:dropper"
    ) || reg.in_tag(state, "minecraft:buttons")
}

fn nearby_redstone(reg: &BReg, dim: DimId, RsPos(x, y, z): RsPos) -> bool {
    // Live devices are in the edit overlay. Check that small neighborhood
    // before asking the generator for conductor and support states.
    let edits = world().lock().unwrap();
    for dx in -2..=2 {
        for dz in -2..=2 {
            let (x, z) = (x + dx, z + dz);
            if let Some(chunk) = edits.get(&(dim, x >> 4, z >> 4)) {
                if (-2..=2).any(|dy| {
                    chunk
                        .get(&local_idx(x, y + dy, z))
                        .is_some_and(|&state| redstone_component(reg, state))
                }) {
                    return true;
                }
            }
        }
    }
    false
}

thread_local! {
    // Block writes can cascade through the legacy neighbor handlers. Queue those
    // origins until the current pass has committed its writes and released its locks.
    static LIVE_REDSTONE_ORIGINS: std::cell::RefCell<Option<VecDeque<(DimId, RsPos)>>> =
        const { std::cell::RefCell::new(None) };
}

fn with_redstone_origins(work: impl FnOnce()) {
    LIVE_REDSTONE_ORIGINS.with(|origins| *origins.borrow_mut() = Some(VecDeque::new()));
    work();
    while let Some((dim, pos)) =
        LIVE_REDSTONE_ORIGINS.with(|origins| origins.borrow_mut().as_mut().unwrap().pop_front())
    {
        run_redstone_pass(dim, pos, None);
    }
    LIVE_REDSTONE_ORIGINS.with(|origins| *origins.borrow_mut() = None);
}

fn live_block_changed(dim: DimId, pos @ RsPos(x, y, z): RsPos, old: u32, new: u32) {
    if old == new {
        return;
    }
    let Some(reg) = breg() else { return };
    if reg.block_id_of(old) != reg.block_id_of(new) {
        if let Some(state) = live_redstone().lock().unwrap().get_mut(&dim) {
            state.comparator_outputs.remove(&pos);
            state.torches.remove(&pos);
            if let Some(id) = reg.block_id_of(old) {
                state.pending.remove(&(pos, id as u32));
            }
        }
        if reg.name_of(old) == "minecraft:brewing_stand" {
            remove_live_brewing_stand((dim, x, y, z));
        }
    }
    if !redstone_component(reg, old)
        && !redstone_component(reg, new)
        && !nearby_redstone(reg, dim, pos)
    {
        return;
    }
    let queued = LIVE_REDSTONE_ORIGINS.with(|origins| {
        if let Some(queue) = origins.borrow_mut().as_mut() {
            queue.push_back((dim, pos));
            true
        } else {
            false
        }
    });
    if !queued {
        with_redstone_origins(|| run_redstone_pass(dim, pos, None));
    }
}

fn run_redstone_pass(dim: DimId, origin: RsPos, scheduled: Option<u32>) {
    let Some(reg) = breg() else { return };
    let (writes, dispensers) = {
        let mut dimensions = live_redstone().lock().unwrap();
        let state = dimensions.entry(dim).or_default();
        let mut torches = std::mem::take(&mut state.torches);
        let now = TICK_IDX.load(Ordering::Relaxed);
        let mut world = LiveRedstoneWorld {
            dim,
            reg,
            state,
            now,
            reads: Default::default(),
            writes: Default::default(),
            dispensers: Vec::new(),
        };
        if let Some(block_id) = scheduled {
            // The canonical sim (`rs_run_tick`) owns the observer pulse; the
            // host toggle would undo its powered edge inside the same pass.
            // Powered buttons have no sim arm and stay host-toggled.
            if !matches!(
                rs_block_from_state(reg, world.state_at(origin)),
                RsBlock::Observer { .. }
            ) {
                world.run_host_tick(origin, block_id);
            }
        } else {
            let state = world.state_at(origin);
            if let RsBlock::Button { powered: true, .. } = rs_block_from_state(reg, state) {
                let delay = if reg.in_tag(state, "minecraft:wooden_buttons") {
                    30
                } else {
                    20
                };
                world.schedule_component(origin, delay);
            }
        }
        let mut adapter = RedstoneWorldAdapter::new(reg, &mut world, &mut torches);
        if let Some(block_id) = scheduled {
            adapter.run_scheduled_tick(origin, block_id, now * u64::from(GAME_TICKS_PER_TICK));
        } else {
            let mut nearby = RsBTreeSet::new();
            rs_extend_neighborhood(&mut nearby, [origin]);
            for pos in nearby {
                adapter.refresh_wire_shape(pos);
            }
            // P6c / #214: the observer fan-out runs the canonical
            // `redstone::rs_notify_block_update`, scheduling each facing
            // observer through the shared pending-tick scheduler.
            rs_notify_block_update(&mut adapter, origin);
        }
        adapter.propagate(&[origin]);
        world.state.torches = torches;
        (world.writes, world.dispensers)
    };
    for (RsPos(x, y, z), state) in writes {
        set_block_if_inner_dim(dim, x, y, z, state, None);
        note_block_update_dim(dim, x, y, z, block_at_dim(dim, x, y, z));
    }
    if dim == DimId::Overworld {
        for RsPos(x, y, z) in dispensers {
            fire_dispenser(x, y, z, block_at(x, y, z));
        }
    }
}

fn live_redstone_tick() {
    with_save_guard(|| {
        let now = TICK_IDX.load(Ordering::Relaxed);
        let mut due = Vec::new();
        for (&dim, state) in live_redstone().lock().unwrap().iter_mut() {
            while let Some(&Reverse((when, _, _, _, _))) = state.ticks.peek() {
                if when > now {
                    break;
                }
                let Reverse((_, _, sequence, pos, id)) = state.ticks.pop().unwrap();
                due.push((dim, sequence, pos, id));
            }
        }
        for (dim, sequence, pos, id) in due {
            let pending = {
                let mut dimensions = live_redstone().lock().unwrap();
                let pending = &mut dimensions.get_mut(&dim).unwrap().pending;
                if pending.get(&(pos, id)) == Some(&sequence) {
                    pending.remove(&(pos, id));
                    true
                } else {
                    false
                }
            };
            if pending {
                with_redstone_origins(|| run_redstone_pass(dim, pos, Some(id)));
            }
        }
    });
}

fn use_redstone_component(c: &Client, (dim, x, y, z): DimBlockPos, state: u32) -> bool {
    let Some(reg) = breg() else { return false };
    let (property, value) = match rs_block_from_state(reg, state) {
        RsBlock::Lever { powered, .. } => ("powered", if powered { "false" } else { "true" }),
        RsBlock::Button { powered: true, .. } => return true,
        RsBlock::Button { .. } => ("powered", "true"),
        RsBlock::Repeater(repeater) => ("delay", ["1", "2", "3", "4"][repeater.delay as usize % 4]),
        RsBlock::Comparator(comparator) => (
            "mode",
            match comparator.mode {
                RsComparatorMode::Compare => "subtract",
                RsComparatorMode::Subtract => "compare",
            },
        ),
        _ => return false,
    };
    if !c.is_spectator() {
        let updated = reg.with_prop(state, property, value);
        if set_block_if_dim(dim, x, y, z, updated, Some(state)) {
            note_block_update_dim(dim, x, y, z, updated);
            if dim == DimId::Overworld && reg.name_of(state) == "minecraft:lever" {
                redstone_update(x, y, z, value == "true");
            }
        }
    }
    true
}

fn live_placement_state(reg: &BReg, item: i64, face: RsDir, yaw: f32, pitch: f32) -> Option<u32> {
    let item_name = item_to_name(item)?;
    let block_name = match item_name {
        "redstone" => "redstone_wire",
        "redstone_torch" if RsDir::HORIZONTAL.contains(&face) => "redstone_wall_torch",
        other => other,
    };
    let state = reg.state_of(&format!("minecraft:{block_name}"), None);
    if !matches!(
        block_name,
        "anvil"
            | "chipped_anvil"
            | "damaged_anvil"
            | "enchanting_table"
            | "brewing_stand"
            | "smithing_table"
            | "bookshelf"
    ) && !redstone_component(reg, state)
    {
        return None;
    }
    let look = [RsDir::South, RsDir::West, RsDir::North, RsDir::East]
        [((yaw / 90.0).round() as i32).rem_euclid(4) as usize];
    let updated = match rs_block_from_state(reg, state) {
        RsBlock::Lever { .. } | RsBlock::Button { .. } => {
            let (mount, facing) = match face {
                RsDir::Up => ("floor", look),
                RsDir::Down => ("ceiling", look),
                wall => ("wall", wall),
            };
            reg.with_prop(
                reg.with_prop(state, "face", mount),
                "facing",
                facing.property(),
            )
        }
        RsBlock::WallTorch(_) => reg.with_prop(state, "facing", face.property()),
        RsBlock::Repeater(_) | RsBlock::Comparator(_) => {
            reg.with_prop(state, "facing", look.opposite().property())
        }
        RsBlock::Observer { .. } => {
            let direction = if pitch > 45.0 {
                RsDir::Down
            } else if pitch < -45.0 {
                RsDir::Up
            } else {
                look
            };
            reg.with_prop(state, "facing", direction.opposite().property())
        }
        _ if matches!(block_name, "anvil" | "chipped_anvil" | "damaged_anvil") => {
            reg.with_prop(state, "facing", look.right().property())
        }
        _ => state,
    };
    Some(updated)
}

fn try_live_block_placement(
    c: &Client,
    (dim, x, y, z): DimBlockPos,
    face: usize,
    held: i16,
    item: i64,
    _against: u32,
) -> bool {
    let Some(reg) = breg() else { return false };
    let (_, _, _, yaw, pitch, _) = *c.pos.lock().unwrap();
    let Some(state) = live_placement_state(reg, item, RsDir::ALL[face], yaw, pitch) else {
        return false;
    };
    let game_mode = *c.gm.lock().unwrap();
    if game_mode == 2 || game_mode == 3 {
        return true;
    }
    let old = block_at_dim(dim, x, y, z);
    if !reg.is_replaceable(old) {
        return true;
    }
    #[cfg(feature = "neoforge-events")]
    if !neoforge_place_allowed(c, (dim, x, y, z), old, state, _against) {
        return true;
    }
    with_save_guard(|| {
        if set_block_if_inner_dim(dim, x, y, z, state, Some(old)) {
            note_block_update_dim(dim, x, y, z, block_at_dim(dim, x, y, z));
            if game_mode != 1 {
                let mut inventory = c.inv.lock().unwrap();
                map_take(&mut inventory, 36 + held, 1);
                let mut packet = vec![0];
                write_varint(&mut packet, 0);
                packet.extend_from_slice(&(36 + held).to_be_bytes());
                packet.extend_from_slice(&slot_wire(
                    inventory.get(&(36 + held)).unwrap_or(&Stack::default()),
                ));
                let _ = send_packet(&mut c.sock.lock().unwrap(), 0x15, packet);
            }
        }
    });
    true
}

// ---------- neighbor-update engine (vanilla-ish: piston QC, tnt ignite, observer, dispenser, gravity) ----------
fn is_power_source(x: i32, y: i32, z: i32) -> bool {
    let st = block_at(x, y, z);
    plate_powered(st)
        || breg().is_some_and(|reg| matches!(
            rs_block_from_state(reg, st),
            RsBlock::RedstoneBlock | RsBlock::Lever { powered: true, .. }
                | RsBlock::Observer { powered: true, .. } | RsBlock::Button { powered: true, .. }
        ))
}
fn piston_powered(px: i32, py: i32, pz: i32) -> bool {
    for (dx, dy, dz) in [
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ] {
        if is_power_source(px + dx, py + dy, pz + dz) {
            return true;
        }
        if is_power_source(px + dx, py + 1 + dy, pz + dz) {
            return true;
        } // QC: cell above's neighbors
        if plate_powered(block_at(px + dx, py + dy + 1, pz + dz)) {
            return true;
        } // plate on the cell
    }
    false
}
fn piston_set_extended(bx: i32, by: i32, bz: i32, st: u32, extended: bool) {
    let lo = if (2011..=2022).contains(&st) {
        2011u32
    } else {
        1992
    };
    let off = st - lo;
    let is_ext = off < 6;
    if is_ext == extended {
        return;
    }
    let facing = (off % 6) as i32;
    let (fx, fy, fz) = match facing {
        0 => (0, 0, -1),
        1 => (1, 0, 0),
        2 => (0, 0, 1),
        3 => (-1, 0, 0),
        4 => (0, 1, 0),
        _ => (0, -1, 0),
    };
    let (px, py, pz) = (bx + fx, by + fy, bz + fz);
    if extended {
        set_block(bx, by, bz, st - 6);
        note_block_update(bx, by, bz, st - 6);
        let front = block_at(px, py, pz);
        let (qx, qy, qz) = (bx + fx * 2, by + fy * 2, bz + fz * 2);
        let head = 2023 + facing as u32 * 4;
        // vanilla: blocks with block entities are immovable — pushing
        // them would orphan/duplicate the contents store
        let immovable = container_kind(front).is_some()
            || (4295..=4302).contains(&front)                          // furnace
            || (9345..=9356).contains(&front)                          // dispenser
            || (523..=534).contains(&front)                            // dropper
            || (9225..=9234).contains(&front)                          // hopper
            || signs()
                .lock()
                .unwrap()
                .keys()
                .any(|&(dim, sx, sy, sz, _)| {
                    dim == DimId::Overworld && sx == px && sy == py && sz == pz
                });
        if front != 0 && !immovable && block_at(qx, qy, qz) == 0 {
            place_block(qx, qy, qz, front);
            place_block(px, py, pz, head);
        } else if front == 0 {
            set_block(px, py, pz, head);
            note_block_update(px, py, pz, head);
        }
        // immovable non-air front: base extends, no head (head can't
        // occupy the block-entity cell) — matches vanilla's no-push
    } else {
        set_block(bx, by, bz, st + 6);
        note_block_update(bx, by, bz, st + 6);
        if (2023..=2046).contains(&block_at(px, py, pz)) {
            set_block(px, py, pz, 0);
            note_block_update(px, py, pz, 0);
        }
    }
}
fn fire_dispenser(dx: i32, dy: i32, dz: i32, st: u32) {
    let Some(reg) = breg() else { return };
    let facing = match rs_block_from_state(reg, st) {
        RsBlock::Dispenser { facing, .. } | RsBlock::Dropper { facing, .. } => facing,
        _ => return,
    };
    let (fx, fy, fz) = facing.offset();
    let (tx, ty, tz) = (dx + fx, dy + fy, dz + fz);
    unpack_block_loot_if_needed(ow_block((dx, dy, dz)));
    let first = chests().lock().unwrap().get(&ContainerKey::ow((dx, dy, dz))).and_then(|m| {
        m.iter()
            .find(|(_, v)| v.1 > 0)
            .map(|(k, v)| (*k, v.clone()))
    });
    let Some((slot, (item, _cnt, _fc))) = first else {
        return;
    };
    match item {
        909 => {
            // water_bucket -> place source
            if block_at(tx, ty, tz) == 0 {
                set_block(tx, ty, tz, 80);
                note_block_update(tx, ty, tz, 80);
            }
        }
        960 => {
            // bone_meal -> grow crop at target
            let cst = block_at(tx, ty, tz);
            if (4278..=4285).contains(&cst) {
                let nst = (cst + 3).min(4285);
                set_block(tx, ty, tz, nst);
                note_block_update(tx, ty, tz, nst);
            } else if (4294..=4301).contains(&cst) {
                let nst = (cst + 3).min(4301);
                set_block(tx, ty, tz, nst);
                note_block_update(tx, ty, tz, nst);
            }
            let mut ch = chests().lock().unwrap();
            if let Some(m) = ch.get_mut(&ContainerKey::ow((dx, dy, dz))) {
                if let Some(v) = m.get_mut(&slot) {
                    v.1 = v.1.saturating_sub(1);
                    if v.1 == 0 {
                        m.remove(&slot);
                    }
                }
            }
        }
        _ => {}
    }
}
static WATERS: OnceLock<Mutex<std::collections::HashSet<(i32, i32, i32)>>> = OnceLock::new();
fn waters() -> &'static Mutex<std::collections::HashSet<(i32, i32, i32)>> {
    WATERS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
static LAVAS: OnceLock<Mutex<std::collections::HashSet<(i32, i32, i32)>>> = OnceLock::new();
fn lavas() -> &'static Mutex<std::collections::HashSet<(i32, i32, i32)>> {
    LAVAS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

thread_local! { static UPD_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) } }
thread_local! { static UPD_DEFER: std::cell::RefCell<Vec<(i32, i32, i32)>> = const { std::cell::RefCell::new(Vec::new()) } }
fn block_updated(x: i32, y: i32, z: i32) {
    sched_invalidate(x, y, z);
    plugin_invalidate(x, y, z);
    let depth = UPD_DEPTH.get();
    // Vanilla has no depth cap on neighbour updates. This bounds the recursion so a very tall
    // column cannot overflow the stack, but instead of discarding the update at the cap it is
    // queued and drained iteratively from the outermost call: a column taller than the cap still
    // converts, instead of leaving its upper blocks suspended forever.
    if depth >= 64 {
        UPD_DEFER.with(|d| d.borrow_mut().push((x, y, z)));
        return;
    }
    UPD_DEPTH.set(depth + 1);
    block_updated_inner(x, y, z);
    UPD_DEPTH.set(depth);
    if depth == 0 {
        while let Some((dx, dy, dz)) = UPD_DEFER.with(|d| d.borrow_mut().pop()) {
            UPD_DEPTH.set(1);
            block_updated_inner(dx, dy, dz);
            UPD_DEPTH.set(0);
        }
    }
}
/// A falling block in flight, as vanilla's `FallingBlockEntity`: the block has left the
/// world, and this owns its state until it lands. `dy` is the vertical velocity and is
/// integrated with vanilla's 0.04-per-tick gravity (`y_n = y0 - 0.02 n(n+1)`).
#[derive(Clone, Debug)]
struct Fall {
    eid: i32,
    x: f64,
    y: f64,
    z: f64,
    dy: f64,
    state: u32,
    time: i32,
}
static FALLING: OnceLock<Mutex<Vec<Fall>>> = OnceLock::new();
fn falling() -> &'static Mutex<Vec<Fall>> {
    FALLING.get_or_init(|| Mutex::new(Vec::new()))
}

/// Whether a block can be fallen into. Vanilla replaces air and fluids; the registry's opacity is
/// the closest thing to hand, and a full block never is.
fn fallable_state(st: u32) -> bool {
    let Some(reg) = crate::block_registry::breg() else {
        return false;
    };
    // Vanilla's collision uses the block's shape, so air, fluids and blocks a player can replace
    // (grass, flowers, torches, rails) have no collision shape: a falling block passes through
    // them and replaces them on landing. `is_replaceable` is the registry's closest predicate.
    reg.is_air(st) || reg.is_liquid(st) || reg.is_replaceable(st)
}
fn fallable(x: i32, y: i32, z: i32) -> bool {
    fallable_state(block_at(x, y, z))
}

/// FallingBlock's neighbor callback reserves a block tick two game ticks in
/// the future, even if a support was just installed. The callback re-reads
/// both the block and its support: placement/replacement can cancel the fall.
/// Use the same scheduled-block queue as leaf ticks, with the same persistent
/// due/priority/suborder and (position, block identity) de-duplication.
/// Booking the changed cell is essential for unsupported gravity placements.
fn note_falling(x: i32, y: i32, z: i32) {
    let Some(reg) = crate::block_registry::breg() else {
        return;
    };
    let due = TICK_IDX.load(Ordering::Relaxed) as i64 + 2;
    for (px, py, pz) in [(x, y, z), (x, y.saturating_add(1), z)] {
        if py <= MIN_Y || py >= MIN_Y + 384 {
            continue;
        }
        let st = block_at(px, py, pz);
        if reg.behaviour_of(st) != crate::block_registry::B_FALLING {
            continue;
        }
        // Reserve first; no world write or falling entity is created until
        // the native-style delayed callback is actually collected.
        let sub_order = BLOCK_TICK_ORDER.fetch_add(1, Ordering::Relaxed);
        block_ticks().lock().unwrap().schedule(scheduled_ticks::ScheduledTick {
            pos: (px, py, pz),
            // Native ScheduledTick de-duplicates per block identity, independent
            // of facing/waterlogged properties of the current state.
            block: reg.state_of(reg.name_of(st), None),
            due,
            priority: 0,
            sub_order,
        });
    }
}

/// Convert a still-unsupported gravity block from its *due* scheduled callback.
/// Removing it schedules the block above for a future two-GT callback; a stacked
/// column must not collapse synchronously before the first callback has run.
fn spawn_fall(px: i32, py: i32, pz: i32, st: u32) {
    // Claim the block with a compare-and-set instead of a check then a clear: two threads can
    // both see the unsupported gravity block, and only the one that actually removes it may
    // create the entity, or one block would fall twice. Keying this on "an entity occupies the
    // cell" was wrong: a falling entity has already left its block cell, so a *new* gravity block
    // placed into that cell matched the passing entity and hung in the air forever.
    with_save_guard(|| {
        if !set_block_if(px, py, pz, 0, Some(st)) {
            return;
        }
        note_block_update(px, py, pz, 0);
        let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
        falling().lock().unwrap().push(Fall {
            eid,
            x: px as f64 + 0.5,
            y: py as f64,
            z: pz as f64 + 0.5,
            dy: 0.0,
            state: st,
            time: 0,
        });
    });
}

/// Place a block and queue its actual state. Falling blocks now remain blocks
/// during the two-tick booking window; only the scheduled callback may turn
/// one into an entity, emitting the intervening air update.
fn place_block(wx: i32, wy: i32, wz: i32, st: u32) {
    place_block_dim(DimId::Overworld, wx, wy, wz, st)
}
fn place_block_dim(dim: DimId, wx: i32, wy: i32, wz: i32, st: u32) {
    set_block_dim(dim, wx, wy, wz, st);
    note_block_update_dim(dim, wx, wy, wz, block_at_dim(dim, wx, wy, wz));
}
/// Advance each falling entity by vanilla's gravity and land it on the first solid block
/// below. The physics is the bytecode-derived contract in evidence/gameplay/falling-vanilla:
/// `dy -= 0.04` per tick, no drag, land when the feet reach the support's top face.
/// The entities to advance this tick. The queue is cloned, not drained, so a save on another
/// thread always sees the complete in-flight set; draining showed it an empty queue and lost
/// blocks that had already left the world.
fn begin_falling_tick() -> (Vec<Fall>, std::collections::HashSet<i32>) {
    let active: Vec<Fall> = falling().lock().unwrap().clone();
    let processed = active.iter().map(|f| f.eid).collect();
    (active, processed)
}

/// Retire the ids this tick handled and re-add the survivors. `retain` keeps any entity a
/// connection thread pushed during the pass, which is why the queue is not simply assigned.
fn end_falling_tick(processed: &std::collections::HashSet<i32>, keep: Vec<Fall>) {
    let mut f = falling().lock().unwrap();
    f.retain(|e| !processed.contains(&e.eid));
    f.extend(keep);
}

fn falling_tick() {
    let (active, processed) = begin_falling_tick();
    let mut keep: Vec<Fall> = Vec::with_capacity(active.len());
    let mut moves: Vec<(i32, f64, f64, f64)> = Vec::new();
    // A landing keeps its entity: the target cell is re-checked at the moment of placement.
    let mut landings: Vec<(Fall, i32, i32, i32)> = Vec::new();
    let mut despawn: Vec<i32> = Vec::new();
    // Items for entities that fell out of the world: created under the same save guard that
    // retires the entity, so a snapshot cannot see both and duplicate the drop.
    let mut below: Vec<Drop> = Vec::new();
    {
        let region = region_guard();
        for mut f in active {
            // A block in flight is an entity in vanilla, so it does not tick in a chunk nobody
            // is near; it stays in flight until the chunk warms up.
            if !region.is_warm(f.x as i32, f.z as i32) {
                keep.push(f);
                continue;
            }
            f.time += 1;
            f.dy -= 0.04;
            let new_y = f.y + f.dy;
            let bx = f.x.floor() as i32;
            let bz = f.z.floor() as i32;
            // The entity is a point, so a tick that moves more than one block must sweep the
            // whole interval: a one-block platform between the old and new feet is a landing,
            // not something to pass through. Scan from the old feet down to the new feet (or the
            // world floor for a fall that leaves the world), before the below-world check.
            let old_floor = f.y.floor() as i32;
            let lowest = (new_y.floor() as i32).max(MIN_Y);
            let mut support = old_floor;
            while support >= lowest && fallable(bx, support, bz) {
                support -= 1;
            }
            if support >= lowest {
                // Defer the placement to the application pass below, which re-checks the cell.
                landings.push((f, bx, support + 1, bz));
                continue;
            }
            if new_y < MIN_Y as f64 {
                // Below the world vanilla cannot place the block, so it drops it rather than
                // destroying the item that left the world when the fall started. The drop is
                // the entity's, so `doEntityDrops` gates it.
                if let Some(item) = block_drop(f.state).filter(|_| gr_do_entity_drops()) {
                    below.push(Drop {
                        eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                        x: f.x,
                        y: f.y,
                        z: f.z,
                        item,
                        cnt: 1,
                        comps: Vec::new(),
                        age: 0,
                        target: None,
                        thrower: None,
                    });
                }
                despawn.push(f.eid);
                continue;
            }
            f.y = new_y;
            moves.push((f.eid, f.x, f.y, f.z));
            keep.push(f);
        }
    }
    with_save_guard(|| {
        if !below.is_empty() {
            drops().lock().unwrap().extend(below);
        }
        end_falling_tick(&processed, keep);
        for (fa, bx, top, bz) in landings {
            // Revalidate and replace the landing cell atomically: a connection thread can
            // place a block after the check, and `set_block` would overwrite it. The
            // compare-and-set fails if the cell changed, and the entity rests on top instead.
            let current = block_at(bx, top, bz);
            if fallable_state(current) && set_block_if(bx, top, bz, fa.state, Some(current)) {
                note_block_update(bx, top, bz, fa.state);
                broadcast_seen_destroy(fa.eid);
            } else {
                // The target is taken: a sibling landed there in this same batch, or a block was
                // placed concurrently. Rest the entity on top of the new block and land it next
                // tick, so neither block is destroyed and the fall terminates.
                let mut rest = fa;
                rest.y = (top + 1) as f64;
                rest.dy = 0.0;
                falling().lock().unwrap().push(rest);
            }
        }
    });
    for (eid, x, y, z) in moves {
        broadcast_seen(eid, |_| {
            (0x70, entity_teleport(eid, x, y, z, 0.0, 0.0, true))
        });
    }
    for eid in despawn {
        broadcast_seen_destroy(eid);
    }
}

/// Support loss is a neighbor update, not a periodic scan. Use the same
/// compare-and-set that protects falling-block creation: repeated notifications
/// and concurrent mutations may not duplicate drops. This covers only the
/// known torch and lantern attachment families (other shapes are separate).
fn note_fragile_supports(x: i32, y: i32, z: i32) {
    let Some(reg) = crate::block_registry::breg() else { return };
    let support = block_at(x, y, z);
    for (dir, (dx, dy, dz)) in LEAF_NEIGHBORS.iter().copied().enumerate() {
        let q = (x.wrapping_add(dx), y.wrapping_add(dy), z.wrapping_add(dz));
        if q.1 < MIN_Y || q.1 >= MIN_Y + 384 {
            continue;
        }
        let state = block_at(q.0, q.1, q.2);
        if !reg.fragile_loses_support(state, (dir as u8) ^ 1, support) {
            continue;
        }
        if set_block_if(q.0, q.1, q.2, 0, Some(state)) {
            note_block_update(q.0, q.1, q.2, 0);
            if let Some(item) = reg
                .block_drop(state)
                .filter(|_| GR_DO_TILE_DROPS.load(Ordering::Relaxed) != 0)
            {
                drops().lock().unwrap().push(Drop {
                    eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                    x: q.0 as f64 + 0.5,
                    y: q.1 as f64 + 0.5,
                    z: q.2 as f64 + 0.5,
                    item,
                    cnt: 1,
                    comps: Vec::new(),
                    age: 0,
                    target: None,
                    thrower: None,
                });
            }
        }
    }
}

/// Update immediate connected-block shapes through the existing world owner.
/// Each actual state change re-enters `block_updated`, which already bounds
/// recursion and drains deep update chains. Never hold a world lock across
/// set_block_if, which itself sends block and packet updates.
fn note_connected_shapes(x: i32, y: i32, z: i32) {
    let Some(reg) = crate::block_registry::breg() else { return };
    let changed = block_at(x, y, z);
    let mut wet = Vec::new();
    if waterlogged_source(changed) {
        wet.push((x, y, z));
    }
    // Native immediate neighbor notifications: WEST, EAST, NORTH, SOUTH, DOWN, UP.
    for (direction, (dx, dy, dz)) in [
        (4u8, (-1, 0, 0)),
        (5, (1, 0, 0)),
        (2, (0, 0, -1)),
        (3, (0, 0, 1)),
        (0, (0, -1, 0)),
        (1, (0, 1, 0)),
    ] {
        let q = (x.wrapping_add(dx), y.wrapping_add(dy), z.wrapping_add(dz));
        if q.1 < MIN_Y || q.1 >= MIN_Y + 384 {
            continue;
        }
        let old = block_at(q.0, q.1, q.2);
        if waterlogged_source(old) {
            wet.push(q);
        }
        let name = reg.name_of(old);
        let above = if name.ends_with("_wall") {
            block_at(q.0, q.1.wrapping_add(1), q.2)
        } else {
            0
        };
        let updated = reg.connection_after_neighbor(old, changed, above, direction ^ 1)
            .or_else(|| {
                if direction <= 1 || !name.ends_with("_stairs") { return None; }
                reg.stair_after_neighbors(old, q, |p| block_at(p.0, p.1, p.2))
            });
        if let Some(new_state) = updated {
            // Avoid stale read/modified/written races: another update may have
            // touched this neighbor after `old` was read.
            set_block_if(q.0, q.1, q.2, new_state, Some(old));
        }
    }
    schedule_waterlogged_positions(wet);
}

fn block_updated_inner(x: i32, y: i32, z: i32) {
    // A block losing its support falls. Checked here rather than by scanning, because this is
    // called exactly when something under it changed.
    note_falling(x, y, z);
    note_fragile_supports(x, y, z);
    note_connected_shapes(x, y, z);
    // piston eval: changed cell + its 6 neighbors
    for (dx, dy, dz) in [
        (0, 0, 0),
        (1, 0, 0),
        (-1, 0, 0),
        (0, 1, 0),
        (0, -1, 0),
        (0, 0, 1),
        (0, 0, -1),
    ] {
        let (bx, by, bz) = (x + dx, y + dy, z + dz);
        let st = block_at(bx, by, bz);
        if (2011..=2022).contains(&st) || (1992..=2003).contains(&st) {
            let p = piston_powered(bx, by, bz);
            piston_set_extended(bx, by, bz, st, p);
        }
        // tnt ignite: tnt next to a power source (direct adjacency — vanilla TNT has no QC)
        if (2094..=2095).contains(&st)
            && [
                (1, 0, 0),
                (-1, 0, 0),
                (0, 1, 0),
                (0, -1, 0),
                (0, 0, 1),
                (0, 0, -1),
            ]
            .iter()
            .any(|(ddx, ddy, ddz)| is_power_source(bx + ddx, by + ddy, bz + ddz))
        {
            set_block(bx, by, bz, 0);
            note_block_update(bx, by, bz, 0);
            let eid = NEXT_EID.fetch_add(1, Ordering::SeqCst);
            primed().lock().unwrap().push(Primed {
                eid,
                x: bx as f64 + 0.5,
                y: by as f64,
                z: bz as f64 + 0.5,
                left: server_ticks(TNT_FUSE_GT) as i32,
            });
            // interest_tick delivers the arrow spawn to trackers
        }
    }
}

fn gc_backoff() {
    // Backoff entries outlive their owners (a dried-up water cell, a
    // removed lamp). Positions no longer tracked by any index can never
    // be scheduled again — drop them so the map stays proportional to
    // live work, not to historical exploration.
    let wtr = waters().lock().unwrap();
    let hh = hoppers().lock().unwrap();
    let cells = plugin_cells().lock().unwrap();
    // waters/hoppers are Overworld-only indexes. A non-Overworld backoff stays live only while a
    // plugin cell in that same dimension names its position; neither may answer for another dim.
    backoff().lock().unwrap().retain(|k, _| {
        let (dim, x, y, z) = *k;
        let plugin_live = cells.iter().any(|((cdim, _, _), bucket)| {
            *cdim == dim && bucket.iter().any(|c| c.x == x && c.y == y && c.z == z)
        });
        (dim == DimId::Overworld && (wtr.contains(&(x, y, z)) || hh.contains(&(x, y, z))))
            || plugin_live
    });
    let mut pb = plugin_backoff().lock().unwrap();
    let live: std::collections::HashSet<CellKey> = cells
        .iter()
        .flat_map(|((dim, _, _), bucket)| {
            bucket
                .iter()
                .map(move |c| (*dim, c.pi, c.x, c.y, c.z, c.key))
        })
        .collect();
    pb.retain(|k, _| live.contains(k));
}



/// Nightly hostile choice: use the pinned biome's weighted monster list for the
/// four types this runtime can simulate, and keep the old rotation only when the
/// biome has no usable entry. This is not NaturalSpawner: placement, group size,
/// difficulty and category caps are still the tick loop's own contract.
#[cfg(test)]
fn spawn_etype_for_biome(biome: Option<&str>, attempt: u64) -> i64 {
    biome
        .and_then(|b| feature::choose_supported_hostile(b, attempt))
        .and_then(name_to_etype)
        .filter(|e| matches!(e, 23 | 91 | 100 | 124))
        .unwrap_or(match attempt % 5 {
            0 => 23,
            1 => 91,
            2 => 100,
            _ => 124,
        })
}

fn name_to_etype(n: &str) -> Option<i64> {
    entity_spec_by_name(n).map(|spec| spec.id)
}

// ---------- resource pack (embedded http + sha1) ----------
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let ml = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, wi) in w.iter().enumerate().take(80) {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for i in 0..5 {
        out[i * 4..i * 4 + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

static PACK_INFO: OnceLock<Option<(String, String)>> = OnceLock::new(); // (url, sha1hex)
fn pack_info() -> &'static Option<(String, String)> {
    PACK_INFO.get_or_init(load_pack_info)
}
fn resource_pack_url(bind: &str, configured: Option<&str>) -> Option<String> {
    if let Some(url) = configured {
        let url = url.trim();
        return (url.starts_with("http://") || url.starts_with("https://")).then(|| url.to_owned());
    }
    // Never suggest 127.0.0.1 to a remote client: that is the client's host.
    match bind {
        "127.0.0.1" => Some("http://127.0.0.1:25598/pack.zip".into()),
        "::1" => Some("http://[::1]:25598/pack.zip".into()),
        _ => None,
    }
}
fn load_pack_info() -> Option<(String, String)> {
    let bind = std::env::var("MC_SPINE_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1".into());
    let url = resource_pack_url(
        &bind,
        std::env::var("MC_SPINE_RESOURCE_PACK_URL").ok().as_deref(),
    )?;
    let data = std::fs::read("resourcepack.zip").ok()?;
    Some((url, hex(&sha1(&data))))
}

fn http_server() {
    if pack_info().is_none() {
        return;
    }
    let Ok(data) = std::fs::read("resourcepack.zip") else {
        return;
    };
    let bind = std::env::var("MC_SPINE_RESOURCE_PACK_BIND_ADDR")
        .or_else(|_| std::env::var("MC_SPINE_BIND_ADDR"))
        .unwrap_or_else(|_| "127.0.0.1".into());
    if let Err(why) = connection_limits::check_bind(
        "resource pack http",
        &bind,
        connection_limits::public_bind_allowed(),
    ) {
        eprintln!("[spine] {why}");
        return;
    }
    let Ok(l) = TcpListener::bind((bind.as_str(), 25598)) else {
        eprintln!("[spine] resource pack bind failed on {bind}:25598");
        return;
    };
    eprintln!(
        "[spine] resource pack http {bind}:25598 ({} bytes)",
        data.len()
    );
    thread::spawn(move || {
        for st in l.incoming().flatten() {
            let mut st = st;
            st.set_read_timeout(Some(Duration::from_secs(5))).ok();
            st.set_write_timeout(Some(Duration::from_secs(10))).ok();
            let mut buf = [0u8; 1024];
            let _ = st.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK
Content-Type: application/zip
Content-Length: {}
Connection: close

",
                data.len()
            );
            let _ = st.write_all(head.as_bytes());
            let _ = st.write_all(&data);
        }
    });
}

// ---------- chunks ----------
/// Slots in the palette writer's stack hash table: a power of two above twice the largest
/// palette a call can build (256 entries), so the probe run at a 50% load factor is short.
const PAL_SLOTS: usize = 512;
/// `PalSlots::vals` marker for a free slot. A palette index never reaches it (max 256).
const PAL_FREE: u16 = u16::MAX;
/// Entries the stack palette has room for: the larger of the two ceilings a caller can reach
/// (`1 << max_bits`, i.e. 256 for block states, 8 for quart biomes), so one shape serves both.
const PAL_MAX_VALUES: usize = 1 << 8;
/// Cells in one section: the stack index buffer is this long. A longer input (a caller passing
/// a slice it built itself) falls back to a `Vec` rather than truncating.
const PAL_STACK_CELLS: usize = 4096;

/// One network palette writer for block states and quart biomes. Values never
/// straddle longs; global mode writes registry IDs, without a local palette.
fn write_palette(
    out: &mut Vec<u8>,
    values: &[i64],
    min_bits: usize,
    max_bits: usize,
    global_bits: usize,
) {
    write_palette_core(
        out,
        values.len(),
        |i| values[i],
        min_bits,
        max_bits,
        global_bits,
    )
}

/// [`write_palette`] over the section's own `[u32; 4096]`: the block-state path does not widen
/// the array into `[i64; 4096]` first, which was a 32 KiB copy per section (24 per chunk).
fn write_palette_u32(
    out: &mut Vec<u8>,
    values: &[u32],
    min_bits: usize,
    max_bits: usize,
    global_bits: usize,
) {
    write_palette_core(
        out,
        values.len(),
        |i| i64::from(values[i]),
        min_bits,
        max_bits,
        global_bits,
    )
}

/// The stack-resident half of the palette writer: value -> palette index by open addressing,
/// and the palette itself in first-seen order.
///
/// This is why an encode allocates nothing. The writer used to build three heap objects per
/// call - a SipHash `HashMap`, a palette `Vec` and an index `Vec` - and a chunk encodes 48
/// arrays (24 sections x block states + quart biomes), so that was 144 allocations plus a
/// SipHash per distinct value per chunk. The work is identical; only the memory moved.
struct PalSlots {
    keys: [i64; PAL_SLOTS],
    vals: [u16; PAL_SLOTS],
    palette: [i64; PAL_MAX_VALUES],
}

/// Where a value sits in the table: an existing palette index, or the free slot it would take.
/// One probe answers both, so a miss costs one pass rather than a lookup followed by an insert.
enum PalProbe {
    Hit(u16),
    Free(usize),
}

#[inline]
fn pal_hash(value: i64) -> usize {
    // A small state id's informative bits are the low ones, so a multiply-mix spreads them and
    // the high bits of the product become the slot. The constant is the 64-bit Weyl
    // (golden-ratio) odd multiplier.
    ((value as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 55) as usize
}

impl PalSlots {
    #[inline]
    fn new() -> Self {
        Self {
            keys: [0; PAL_SLOTS],
            vals: [PAL_FREE; PAL_SLOTS],
            palette: [0; PAL_MAX_VALUES],
        }
    }

    #[inline]
    fn probe(&self, value: i64) -> PalProbe {
        let mut slot = pal_hash(value);
        loop {
            let index = self.vals[slot];
            if index == PAL_FREE {
                return PalProbe::Free(slot);
            }
            if self.keys[slot] == value {
                return PalProbe::Hit(index);
            }
            slot = (slot + 1) & (PAL_SLOTS - 1);
        }
    }
}

/// Bits a palette of `distinct` entries needs: `ceil(log2(distinct))`, as before.
#[inline]
fn palette_bits(distinct: usize) -> usize {
    (usize::BITS - (distinct - 1).leading_zeros()) as usize
}

/// One non-straddling u64 word per `64 / bits` cells, zero-padded in the trailing slots.
/// `entry` returns the value to pack: a palette index for the indirect form, the state itself
/// for the direct one. Neither is masked to `bits` - the caller's ids fit, and masking here
/// would be a behaviour change for a value that did not.
#[inline]
fn write_packed_words(
    out: &mut Vec<u8>,
    bits: usize,
    len: usize,
    mut entry: impl FnMut(usize) -> u64,
) {
    let per = 64 / bits;
    write_varint(out, len.div_ceil(per) as i64);
    let mut base = 0usize;
    while base < len {
        let end = (base + per).min(len);
        let mut word = 0u64;
        for cell in base..end {
            word |= entry(cell) << ((cell - base) * bits);
        }
        out.extend_from_slice(&word.to_be_bytes());
        base = end;
    }
}

/// The writer both palette entry points share. `get` reads the n-th value: `[i64]` for quart
/// biomes, `[u32]` for block states.
fn write_palette_core(
    out: &mut Vec<u8>,
    len: usize,
    get: impl Fn(usize) -> i64,
    min_bits: usize,
    max_bits: usize,
    global_bits: usize,
) {
    // No values: there is no palette to build and nothing to index. The allocating writer took
    // its direct branch here by wrapping `len - 1`; say it rather than rely on the wrap.
    if len == 0 {
        out.push(global_bits as u8);
        write_varint(out, 0);
        return;
    }
    // One value: no table, no index buffer, no allocation. A chunk's 24 sections are mostly
    // uniform (all air above the surface, all stone below it), so this path carries most of
    // the encodes.
    let first = get(0);
    let mut uniform = true;
    for i in 1..len {
        if get(i) != first {
            uniform = false;
            break;
        }
    }
    if uniform {
        out.push(0);
        write_varint(out, first);
        write_varint(out, 0);
        return;
    }
    // The indirect form's ceiling: 256 entries for block states (`max_bits = 8`), 8 for quart
    // biomes (`max_bits = 3`). Past it the section is packed directly, with no local palette.
    //
    // The assert is the invariant the stack buffers rest on: a wider palette would overrun
    // `palette` and fill the probe table past the load factor that bounds it. It is a branch on
    // a constant at every call site.
    assert!(
        max_bits <= 8,
        "the stack palette holds {PAL_MAX_VALUES} entries, so max_bits may not exceed 8"
    );
    let max_distinct = 1usize << max_bits;
    let mut table = PalSlots::new();
    let mut stack = [0u16; PAL_STACK_CELLS];
    let mut heap: Vec<u16> = Vec::new();
    let indices: &mut [u16] = if len <= PAL_STACK_CELLS {
        &mut stack[..len]
    } else {
        heap.resize(len, 0);
        &mut heap[..]
    };
    let mut distinct = 0usize;
    // Stop building the palette the moment it cannot address the section: the direct form
    // needs no indices at all, so abandoning the buffer here saves the rest of the pass.
    let mut direct = false;
    for (i, slot) in indices.iter_mut().enumerate() {
        let value = get(i);
        match table.probe(value) {
            PalProbe::Hit(index) => *slot = index,
            PalProbe::Free(free) => {
                if distinct == max_distinct {
                    direct = true;
                    break;
                }
                table.keys[free] = value;
                table.vals[free] = distinct as u16;
                table.palette[distinct] = value;
                *slot = distinct as u16;
                distinct += 1;
            }
        }
    }
    // At most `max_distinct` entries reach here, so the width cannot exceed `max_bits`; the
    // min-clamp is what lifts a tiny palette (2..=8 block states, 2..=4 biomes) to 4 and 1 bits.
    let indirect_bits = palette_bits(distinct).max(min_bits);
    let direct = direct || indirect_bits > max_bits;
    let bits = if direct { global_bits } else { indirect_bits };
    out.push(bits as u8);
    if !direct {
        write_varint(out, distinct as i64);
        for &value in &table.palette[..distinct] {
            write_varint(out, value);
        }
        write_packed_words(out, bits, len, |cell| u64::from(indices[cell]));
    } else {
        write_packed_words(out, bits, len, |cell| get(cell) as u64);
    }
}

/// One section's payload, appended to `out`: the non-air count, the block-state palette and the
/// quart-biome palette. Writing into a caller's buffer is what keeps the 48 default-sized
/// `Vec`s a chunk used to allocate out of the delivery path.
fn section_encode_into(out: &mut Vec<u8>, states: &[u32; 4096], biomes: &[i64; 64]) {
    // Protocol-767 air/void_air/cave_air IDs; do not depend on optional
    // cross-version metadata to decide the section's wire count.
    let non_air = states
        .iter()
        .filter(|&&state| !matches!(state, 0 | 12958 | 12959))
        .count() as i16;
    out.extend_from_slice(&non_air.to_be_bytes());
    // The pinned registry has 26684 block states, requiring fifteen global bits.
    write_palette_u32(out, states, 4, 8, 15);
    // The protocol-767 configuration registry has 64 biome IDs (six global bits).
    write_palette(out, biomes, 1, 3, 6);
}

fn section_encode(states: &[u32; 4096], biomes: &[i64; 64]) -> Vec<u8> {
    let mut out = Vec::new();
    section_encode_into(&mut out, states, biomes);
    out
}

/// One 2048-byte sky light array per section, in the client's layout, from a per-column
/// floor. A column's light is 15 above `floors[i]` and 0 at and below it.
///
/// Taking the floors as an argument rather than reading the world is deliberate: the first
/// attempt at this computed them internally, came out inverted, and could not be reasoned
/// about because the test depended on world state as well. With the input explicit the test
/// can hand it floors it chose.
///
/// VERTICAL ONLY. Vanilla also spreads sky light sideways, so this lights the column that
/// has sky directly above it and nothing else: a window or a cave mouth gives no light
/// through the opening. That is a known limit, not parity.
/// Block light from the given sources, packed for the centre chunk.
///
/// The spread runs over the three-by-three chunk area around the centre, which is exact
/// rather than generous: light falls by one a step, so nothing sixteen blocks away can
/// reach the centre chunk at all. Sources within one chunk would leave a dark seam along
/// every border.
///
/// With no sources the map stays empty, the spread does nothing, and the cost is the
/// call itself. That is the common case and it is why this can go in without a measurement
/// showing a regression.
/// The area a block light spread may use: the centre chunk with one chunk of margin, which is
/// where light from a neighbouring source can reach.
/// The y window is the dimension's own `min_y..height`,
/// so a Nether source at y=32 is not silently clipped by the Overworld's -64 floor and an
/// Overworld cell below 0 is not lit by a Nether emitter.
fn within_search_dim(dim: DimId, cx: i32, cz: i32, x: i32, y: i32, z: i32) -> bool {
    x >= cx * 16 - 15
        && x <= cx * 16 + 30
        && z >= cz * 16 - 15
        && z <= cz * 16 + 30
        && (dim.min_y()..dim.min_y() + dim.height()).contains(&y)
}

/// Light-emitting blocks the **generator** placed in a chunk, recorded by `chunk_data` as it
/// computes every state. They are not in the overlay, so the block-light spread cannot find them
/// by scanning edits; without this a generated lava lake or sea lantern emits no light.
///
/// Owned by the connection's chunk window: the light pass only ever needs the sources of a chunk
/// and its eight neighbours, all of which that client holds, so this is one entry per held chunk
/// and is dropped when a chunk unloads. That keeps it bounded and, together with the light
/// update sent when a later neighbour is generated, order-independent.
pub(crate) type GenSources = HashMap<(i32, i32), Vec<((i32, i32, i32), u8)>>;

/// The first light section, from vanilla `LevelLightEngine.getMinLightSection()`:
/// `getMinSection() - 1`, one section below the world's lowest block section. The light packet
/// sends `getLightSectionCount() = getSectionsCount() + 2` sections from here, so the array at
/// index i belongs to light section `LIGHT_MIN_Y/16 + i`. Anchoring at `MIN_Y` instead would
/// shift every array up one section.
#[allow(dead_code)] // cfg(test) fixtures anchor light sections here
const LIGHT_MIN_Y: i32 = MIN_Y - 16;

#[cfg(test)]
fn block_light_sections_from(
    cx: i32,
    cz: i32,
    sources: &[((i32, i32, i32), u8)],
    opacity: impl Fn(i32, i32, i32) -> u8,
) -> Vec<[u8; 2048]> {
    block_light_sections_from_dim(DimId::Overworld, cx, cz, sources, opacity)
}

fn block_light_sections_from_dim(
    dim: DimId,
    cx: i32,
    cz: i32,
    sources: &[((i32, i32, i32), u8)],
    opacity: impl Fn(i32, i32, i32) -> u8,
) -> Vec<[u8; 2048]> {
    let light_sections = dim.light_sections();
    let light_min_y = dim.light_min_y();
    let live: Vec<((i32, i32, i32), u8)> = sources
        .iter()
        .copied()
        .filter(|(p, l)| *l > 0 && within_search_dim(dim, cx, cz, p.0, p.1, p.2))
        .collect();
    if live.is_empty() {
        return vec![[0u8; 2048]; light_sections];
    }
    // A dense box rather than a hash map. The spread visits every cell within fifteen of a
    // source, so a hash of the position for each of tens of thousands of visits costs more than
    // the arithmetic the map was there to save.
    let (mut bx0, mut bx1) = (i32::MAX, i32::MIN);
    let (mut by0, mut by1) = (i32::MAX, i32::MIN);
    let (mut bz0, mut bz1) = (i32::MAX, i32::MIN);
    for (p, _) in &live {
        bx0 = bx0.min(p.0);
        bx1 = bx1.max(p.0);
        by0 = by0.min(p.1);
        by1 = by1.max(p.1);
        bz0 = bz0.min(p.2);
        bz1 = bz1.max(p.2);
    }
    // Fifteen in every direction is the reach of the brightest source, so nothing outside the
    // expanded box can be lit. The search area clips it, as the map form did.
    let x0 = (bx0 - 15).max(cx * 16 - 15);
    let x1 = (bx1 + 15).min(cx * 16 + 30);
    let z0 = (bz0 - 15).max(cz * 16 - 15);
    let z1 = (bz1 + 15).min(cz * 16 + 30);
    let y0 = (by0 - 15).max(dim.min_y());
    let y1 = (by1 + 15).min(dim.min_y() + dim.height() - 1);
    let (w, d, h) = (
        (x1 - x0 + 1) as usize,
        (z1 - z0 + 1) as usize,
        (y1 - y0 + 1) as usize,
    );
    let mut light = vec![0u8; w * d * h];
    // Opacity, read once per cell rather than once per neighbour. A visited cell is asked about
    // by each of its six neighbours, so the spread was reading the world six times for values it
    // had already seen. 0xff means "not looked up yet"; opacity is 0..15.
    let mut op = vec![0xffu8; w * d * h];
    let idx = |x: i32, y: i32, z: i32| {
        ((y - y0) as usize * d + (z - z0) as usize) * w + (x - x0) as usize
    };
    let mut queue: std::collections::VecDeque<((i32, i32, i32), u8)> =
        std::collections::VecDeque::new();
    for (p, l) in &live {
        let i = idx(p.0, p.1, p.2);
        if light[i] < *l {
            light[i] = *l;
            queue.push_back((*p, *l));
        }
    }
    while let Some((p, level)) = queue.pop_front() {
        if level <= 1 {
            continue;
        }
        for (dx, dy, dz) in [
            (1, 0, 0),
            (-1, 0, 0),
            (0, 1, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
        ] {
            let (qx, qy, qz) = (p.0 + dx, p.1 + dy, p.2 + dz);
            if qx < x0 || qx > x1 || qz < z0 || qz > z1 || qy < y0 || qy > y1 {
                continue;
            }
            let i = idx(qx, qy, qz);
            let o = if op[i] == 0xff {
                let v = opacity(qx, qy, qz);
                op[i] = v;
                v
            } else {
                op[i]
            };
            let nl = level.saturating_sub(o.max(1));
            if nl == 0 {
                continue;
            }
            if light[i] >= nl {
                continue;
            }
            light[i] = nl;
            queue.push_back(((qx, qy, qz), nl));
        }
    }
    let mut out = Vec::with_capacity(light_sections);
    for sec in 0..light_sections {
        let sy = (sec as i32) * 16 + light_min_y;
        out.push(pack_light_section(sy, |lx, wy, lz| {
            let (wx, wz) = (cx * 16 + lx as i32, cz * 16 + lz as i32);
            // The box only covers the sources' fifteen-block reach, so a chunk column the box
            // does not span (all sources on the far side) is dark; without this the index
            // arithmetic underflows on the columns outside the box.
            if wx < x0 || wx > x1 || wz < z0 || wz > z1 || wy < y0 || wy > y1 {
                return 0;
            }
            light[idx(wx, wy, wz)]
        }));
    }
    out
}

/// The sources a chunk can see: blocks the world holds that emit light, plus the lava the
/// server already tracks. Generated glowstone and similar are not found, because the only
/// list of what the world contains is the overlay of edits; that limit is recorded rather
/// than papered over by scanning every block.
fn block_light_sections(cx: i32, cz: i32, gen: &GenSources) -> Vec<[u8; 2048]> {
    block_light_sections_dim(DimId::Overworld, cx, cz, gen)
}

fn block_light_sections_dim(dim: DimId, cx: i32, cz: i32, gen: &GenSources) -> Vec<[u8; 2048]> {
    if let Some(light) = settled_live_light(dim, cx, cz) {
        return light.block.clone();
    }
    let light_sections = dim.light_sections();
    let Some(reg) = crate::block_registry::breg() else {
        return vec![[0u8; 2048]; light_sections];
    };
    // The lava set is server-tracked for the Overworld only. It is deliberately not read for
    // the Nether/End: a lava block a player placed in the Overworld must not phantom-light a
    // Nether chunk at the same coordinate. Generated Nether lava oceans are in `gen` instead.
    // It is still taken first and released before the world lock, so the lock order holds.
    let lava: Vec<(i32, i32, i32)> = if dim == DimId::Overworld {
        lavas().lock().unwrap().iter().copied().collect()
    } else {
        Vec::new()
    };
    // Sources are collected under one short lock, **by reference**: the 3x3 window's edit maps
    // are copied only once something has to be lit.
    //
    // This pass used to hold `world()` from here to the end: it reads a cell per neighbour
    // per visited cell (tens of thousands of reads for a single torch), and taking the
    // mutex per read was what made this the expensive half of the light work. Holding it
    // instead meant every tick-thread read queued behind a whole light pass. The copy is
    // the window's own edit maps — a handful of entries per chunk, empty on fresh chunks.
    //
    // The copy is skipped entirely when the window holds no emitter, which is the common case
    // and the one the call is measured on: a fresh chunk has no edits at all, so the nine
    // clones bought nothing. So the scan runs first, over the maps themselves, and the maps
    // are cloned afterwards only for the spread's opacity lookups.
    let mut sources: Vec<((i32, i32, i32), u8)> = Vec::new();
    let mut w: HashMap<(DimId, i32, i32), HashMap<u32, u32>> = HashMap::new();
    {
        let guard = world().lock().unwrap();
        // Generated light sources for this chunk and the eight neighbours (recorded by
        // `chunk_data` as each was streamed). `block_light_sections_from` clips them to the
        // search area, so an emitter across a border lights this chunk exactly as if it were
        // inside it. A cell the overlay now holds is skipped: the overlay scan below decides
        // its live emission, so a generated lava the player has since removed does not light
        // again.
        for dx in -1..=1 {
            for dz in -1..=1 {
                let Some(g) = gen.get(&(cx + dx, cz + dz)) else {
                    continue;
                };
                for (p, lum) in g {
                    if guard
                        .get(&(dim, p.0 >> 4, p.2 >> 4))
                        .and_then(|c| c.get(&local_idx(p.0, p.1, p.2)))
                        .is_some()
                    {
                        continue;
                    }
                    sources.push((*p, *lum));
                }
            }
        }
        for dx in -1..=1 {
            for dz in -1..=1 {
                let (ncx, ncz) = (cx + dx, cz + dz);
                let Some(col) = guard.get(&(dim, ncx, ncz)) else {
                    continue;
                };
                for (idx, st) in col {
                    let lum = reg.luminance_of(*st);
                    if lum == 0 {
                        continue;
                    }
                    let x = ncx * 16 + (*idx & 15) as i32;
                    let z = ncz * 16 + ((*idx >> 4) & 15) as i32;
                    let y = (*idx >> 8) as i32 + MIN_Y;
                    sources.push(((x, y, z), lum));
                }
            }
        }
        // The tracked lava is a source only inside this chunk's search area, which is exactly
        // what `block_light_sections_from` would filter it to; dropping it here instead means a
        // chunk whose only candidate is remote lava takes the empty path below.
        for (x, y, z) in &lava {
            if within_search_dim(dim, cx, cz, *x, *y, *z) {
                sources.push(((*x, *y, *z), 15));
            }
        }
        // Nothing to light: return before the window is copied and before the height map is
        // built. Building the height map unconditionally cost this call 45 microseconds on a
        // chunk with no source, against 5.5 before, which is the common case - measured, and
        // the reason the early return is here rather than inside the spread.
        if sources.is_empty() {
            drop(guard);
            return vec![[0u8; 2048]; light_sections];
        }
        for dx in -1..=1 {
            for dz in -1..=1 {
                let key = (cx + dx, cz + dz);
                if let Some(col) = guard.get(&(dim, key.0, key.1)) {
                    if !col.is_empty() {
                        w.insert((dim, key.0, key.1), col.clone());
                    }
                }
            }
        }
    }
    // The heights for the search area, once. A dense row per z rather than a map: the closure
    // is called six times per visited cell, and a hash lookup there would cost what the
    // height lookup it replaces did.
    let (hx0, hx1) = (cx * 16 - 15, cx * 16 + 30);
    let (hz0, hz1) = (cz * 16 - 15, cz * 16 + 30);
    let hw = (hx1 - hx0 + 1) as usize;
    let hd = (hz1 - hz0 + 1) as usize;
    let mut heights = Vec::with_capacity(hw * hd);
    for z in hz0..=hz1 {
        for x in hx0..=hx1 {
            heights.push(height_at_dim(dim, x, z));
        }
    }
    let height_of = |x: i32, z: i32| -> i32 {
        let (ix, iz) = ((x - hx0) as usize, (z - hz0) as usize);
        if ix < hw && iz < hd {
            heights[iz * hw + ix]
        } else {
            height_at_dim(dim, x, z)
        }
    };
    block_light_sections_from_dim(dim, cx, cz, &sources, |x, y, z| {
        let h = height_of(x, z);
        let st = w
            .get(&(dim, x >> 4, z >> 4))
            .and_then(|c| c.get(&local_idx(x, y, z)).copied())
            .unwrap_or_else(|| {
                if dim == DimId::Overworld {
                    base_state_h(x, y, z, h)
                } else {
                    base_state_dim(dim, x, y, z)
                }
            });
        reg.opacity_of(st)
    })
}

#[cfg(test)]
fn sky_light_sections(cx: i32, cz: i32) -> Vec<[u8; 2048]> {
    let reg = crate::block_registry::breg();
    let mut floors = [0i32; 256];
    for lz in 0..16usize {
        for lx in 0..16usize {
            let (x, z) = (cx * 16 + lx as i32, cz * 16 + lz as i32);
            // Walk down from the surface while the block lets light through, so water,
            // glass and leaves near the top do not shade what is under them.
            let mut y = height_at(x, z) - 1;
            let mut steps = 0;
            while steps < 16 {
                let st = block_at(x, y, z);
                if reg.map(|r| r.opacity_of(st)).unwrap_or(0) > 0 {
                    break;
                }
                y -= 1;
                steps += 1;
            }
            floors[(lz << 4) | lx] = y;
        }
    }
    sky_light_sections_from(cx, cz, &floors)
}

/// Sky light that arrives sideways, which the per-column rule cannot produce: a window or a cave
/// mouth lights what is beside it and a column test lights only what is above it.
///
/// The seed is the lit cell just above each column's floor (the cell the per-column rule lights).
/// A cave with a solid ceiling seeds from the cell above the ceiling, which is its own sky; the
/// light that reaches inside it arrives through the opening, and the BFS only enters dark cells.
///
/// The result is a **dense flat grid**, not a `HashMap<(i32, i32, i32), u8>`. The BFS visits
/// tens of thousands of cells and the map hashing cost what the arithmetic it saved did — the
/// same conclusion `block_light_sections_from` reached and wrote down. The box is exact rather
/// than generous: a seed starts at level 15, every step costs at least one, and a level of 1 or
/// less stops, so nothing more than fourteen blocks from a seed can be lit, and nothing more
/// than fifteen can be reached horizontally (`W = D = 46`, the centre chunk plus one of margin).
struct SkySpill {
    x0: i32,
    z0: i32,
    y0: i32,
    w: usize,
    d: usize,
    #[cfg(test)]
    h: usize,
    light: Vec<u8>,
    /// Every cell the spread lit, as a flat index and its level, in the order it was set. The
    /// applier walks this rather than the whole box: the box is one cell per layer of the 3x3
    /// window, and most of it stays dark, so scanning it cost more than the BFS saved.
    lit: Vec<(u32, u8)>,
}

impl Default for SkySpill {
    /// No box: a grid that holds nothing and applies nothing. Used by the mutation campaign,
    /// which replaces the build with `Default::default()` and requires the light assembly test
    /// to notice the missing spill.
    fn default() -> Self {
        Self {
            x0: 0,
            z0: 0,
            y0: 0,
            w: 0,
            d: 0,
            #[cfg(test)]
            h: 0,
            light: Vec::new(),
            lit: Vec::new(),
        }
    }
}

impl SkySpill {
    #[inline]
    fn index(&self, x: i32, y: i32, z: i32) -> usize {
        (((y - self.y0) as usize) * self.d + ((z - self.z0) as usize)) * self.w
            + ((x - self.x0) as usize)
    }

    /// The level at a world position, `None` outside the box. A cell inside it that no seed or
    /// step reached reads as `Some(&0)`, so a caller merging levels sees no light either way.
    /// The map-shaped accessor the spill's own behavioural test drives, kept so that test states
    /// positions rather than flat indices.
    #[cfg(test)]
    fn get(&self, key: &(i32, i32, i32)) -> Option<&u8> {
        let (x, y, z) = *key;
        if self.w == 0
            || !(self.x0..self.x0 + self.w as i32).contains(&x)
            || !(self.z0..self.z0 + self.d as i32).contains(&z)
            || !(self.y0..self.y0 + self.h as i32).contains(&y)
        {
            return None;
        }
        self.light.get(self.index(x, y, z))
    }

    /// Raise the centre chunk's packed nibbles to this grid's levels.
    fn raise_into(&self, dim: DimId, cx: i32, cz: i32, out: &mut [[u8; 2048]]) {
        let plane = self.w * self.d;
        for &(at, level) in &self.lit {
            let at = at as usize;
            let x = self.x0 + (at % self.w) as i32;
            let z = self.z0 + ((at / self.w) % self.d) as i32;
            let y = self.y0 + (at / plane) as i32;
            raise_sky_nibble(dim, out, cx, cz, x, y, z, level);
        }
    }
}

/// Raise one cell's nibble in the packed light-section output. The single owner of the
/// layout arithmetic: both the map-shaped composition the tests drive and the dense grid above
/// write through it.
fn raise_sky_nibble(
    dim: DimId,
    out: &mut [[u8; 2048]],
    cx: i32,
    cz: i32,
    x: i32,
    y: i32,
    z: i32,
    level: u8,
) {
    let light_min_y = dim.light_min_y();
    if x.div_euclid(16) != cx
        || z.div_euclid(16) != cz
        || !(light_min_y..light_min_y + out.len() as i32 * 16).contains(&y)
    {
        return;
    }
    let (lx, lz, relative_y) = (
        (x & 15) as usize,
        (z & 15) as usize,
        (y - light_min_y) as usize,
    );
    let byte = &mut out[relative_y / 16][(relative_y & 15) * 128 + lz * 8 + lx / 2];
    let shift = (lx & 1) * 4;
    let raised = ((*byte >> shift) & 15).max(level) & 15;
    *byte = (*byte & !(15 << shift)) | (raised << shift);
}

fn sky_spill(
    dim: DimId,
    cx: i32,
    cz: i32,
    floor_of: impl Fn(i32, i32) -> i32,
    opacity: impl Fn(i32, i32, i32) -> u8,
) -> SkySpill {
    let (x0, x1) = (cx * 16 - 15, cx * 16 + 30);
    let (z0, z1) = (cz * 16 - 15, cz * 16 + 30);
    let (w, d) = ((x1 - x0 + 1) as usize, (z1 - z0 + 1) as usize);
    // The box has to hold every seed before it can be sized, and the seeds are the centre
    // chunk's own columns (256 of them). Two passes over the columns rather than a `Vec` of
    // seeds: `floor_of` is a read of a cached floor array.
    let (mut smin, mut smax) = (i32::MAX, i32::MIN);
    for lz in 0..16usize {
        for lx in 0..16usize {
            let y = floor_of(cx * 16 + lx as i32, cz * 16 + lz as i32) + 1;
            smin = smin.min(y);
            smax = smax.max(y);
        }
    }
    // Fourteen steps of reach in both directions, clipped to the world — plus the seeds
    // themselves, which a floor outside the world's range can put a block past that clip.
    let y0 = (smin.saturating_sub(14).max(dim.min_y())).min(smin);
    let y1 = (smax
        .saturating_add(14)
        .min(dim.min_y() + dim.height() - 1))
    .max(smax);
    let h = (y1 - y0 + 1) as usize;
    let dark = |x: i32, y: i32, z: i32| -> bool { y <= floor_of(x, z) };
    let mut grid = SkySpill {
        x0,
        z0,
        y0,
        w,
        d,
        #[cfg(test)]
        h,
        light: vec![0u8; w * d * h],
        lit: Vec::new(),
    };
    // Flat index plus level, so a queue entry is five bytes of payload instead of a position
    // triple: the queue holds tens of thousands of these at the worst point of a spread.
    let mut queue: std::collections::VecDeque<(u32, u8)> = std::collections::VecDeque::new();
    for lz in 0..16usize {
        for lx in 0..16usize {
            let (x, z) = (cx * 16 + lx as i32, cz * 16 + lz as i32);
            let y = floor_of(x, z) + 1;
            // Every column seeds: the floor+1 cell is never dark by the `y <= floor` test, so the
            // old `if dark(..) { continue }` guard was dead. The per-column rule already lights
            // this cell to 15; the seed exists so the BFS has a source to spread sideways from.
            let i = grid.index(x, y, z);
            grid.light[i] = 15;
            grid.lit.push((i as u32, 15));
            queue.push_back((i as u32, 15));
        }
    }
    while let Some((at, level)) = queue.pop_front() {
        if level <= 1 {
            continue;
        }
        let at = at as usize;
        let x = x0 + (at % w) as i32;
        let z = z0 + ((at / w) % d) as i32;
        let y = y0 + (at / (w * d)) as i32;
        for (dx, dy, dz) in [
            (1, 0, 0),
            (-1, 0, 0),
            (0, 1, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
        ] {
            let (qx, qy, qz) = (x + dx, y + dy, z + dz);
            if !(x0..=x1).contains(&qx)
                || !(z0..=z1).contains(&qz)
                || !(y0..=y1).contains(&qy)
                || !(dim.min_y()..dim.min_y() + dim.height()).contains(&qy)
            {
                continue;
            }
            if !dark(qx, qy, qz) {
                continue;
            }
            let nl = level.saturating_sub(opacity(qx, qy, qz).max(1));
            if nl == 0 {
                continue;
            }
            let i = grid.index(qx, qy, qz);
            if grid.light[i] >= nl {
                continue;
            }
            grid.light[i] = nl;
            grid.lit.push((i as u32, nl));
            queue.push_back((i as u32, nl));
        }
    }
    grid
}

/// Build the spill and raise it into an already-packed centre chunk, without a map in between.
fn sky_spill_apply(
    dim: DimId,
    cx: i32,
    cz: i32,
    floor_of: impl Fn(i32, i32) -> i32,
    op: impl Fn(i32, i32, i32) -> u8,
    out: &mut [[u8; 2048]],
) {
    let spill = sky_spill(dim, cx, cz, floor_of, op);
    // The type is pinned on its own line rather than at the call. `tools/mutation_check.sh`
    // replaces the line above with `Default::default()` and requires the light assembly test to
    // go red; a `default()` whose type is only fixed by a method call on it is an inference
    // error (E0282), and a mutant that does not build proves nothing.
    let grid: SkySpill = spill;
    grid.raise_into(dim, cx, cz, out);
}

/// The first opaque block y in each column, scanning down from the surface (the sky floor).
/// Opacity for the cells the light computation reads, per chunk, read in batches.
///
/// Measured with `MC_SPINE_CHUNK_PROFILE=1`: one delivered chunk makes **21,619-29,536**
/// `block_at` calls, ~100% of which miss the overlay (it holds edits only) and fall
/// through to `base_state` → `worldgen::block_state` — a full terrain evaluation with
/// the worldgen lock. At 1-2 µs each that is the whole 25-50 ms the light phase costs.
///
/// The cells the light pass reads all sit near a column's surface (the sky floor scan
/// walks from the surface down, the spill BFS spreads ±15 around a floor), so they can
/// be read as a band per chunk instead: one batched terrain pass per overlapping
/// section, the chunk's overlay applied on top. The band is `[min wtop - 18,
/// max wtop + 18]`, which is the range both users reach into.
///
/// Invalidation is the same world-wide edit epoch the floor cache uses: any write into
/// `world()` drops every band, because a band describes blocks.
#[derive(Clone)]
/// A chunk's cells in the y band its readers touch, as block states.
///
/// This is the port's substitute for vanilla's per-chunk block array: without it every
/// read of a generated cell is a full worldgen evaluation (`base_state` → density +
/// material rule + aquifer, with the worldgen lock). The band covers what the light pass
/// and the scheduled-tick pass actually read — `[min wtop - 18, max wtop + 18]` — so a
/// leaf's distance update, a torch's spread and a sky-floor scan are array reads.
///
/// It is built by `chunk_data` from the states it has just evaluated (`install_band_rows`),
/// so a streamed chunk costs nothing extra; a chunk that has not been streamed yet is
/// built on demand from `terrain_section`.
struct OpBand {
    y0: i32,
    y1: i32,
    cells: Vec<u32>,
}

/// Caches that describe a generator must not outlive it: `band_state` and `stored_state`
/// answer only for bands published under the current `worldgen::generation_seq`.
fn current_gen_seq() -> u64 {
    worldgen::generation_seq()
}

impl OpBand {
    fn get(&self, y: i32, lx: usize, lz: usize) -> Option<u32> {
        if y < self.y0 || y > self.y1 {
            return None;
        }
        Some(self.cells[(((y - self.y0) as usize) << 8) | (lz << 4) | lx])
    }
}

/// Band cache: a hash index plus insertion order for eviction.
///
/// The first version was a `VecDeque` scanned linearly, which measured ~0.4 µs per
/// lookup — and the scheduled-tick pass asks for ~28,000 cells per tick, so the lookup
/// alone was most of its 12.5 ms. A `HashMap` makes it a hash hit; the `order` deque only
/// exists for the bound.
struct BandCache {
    map: HashMap<(DimId, i32, i32), (u64, std::sync::Arc<OpBand>)>,
    order: std::collections::VecDeque<(DimId, i32, i32)>,
    generation: u64,
}

static OP_BANDS: OnceLock<Mutex<BandCache>> = OnceLock::new();

fn op_bands() -> &'static Mutex<BandCache> {
    OP_BANDS.get_or_init(|| {
        Mutex::new(BandCache {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
            generation: current_gen_seq(),
        })
    })
}

impl BandCache {
    /// Drop everything if the generator has been replaced since the last entry was stored.
    fn sync_generation(&mut self) {
        let seq = current_gen_seq();
        if self.generation != seq {
            self.map.clear();
            self.order.clear();
            self.generation = seq;
        }
    }

    fn get(&mut self, key: (DimId, i32, i32)) -> Option<std::sync::Arc<OpBand>> {
        self.sync_generation();
        self.map.get(&key).map(|(_, b)| b.clone())
    }

    /// Insert (or replace) and evict the oldest entry when the bound is passed.
    fn put(&mut self, key: (DimId, i32, i32), version: u64, band: std::sync::Arc<OpBand>) {
        if self.map.insert(key, (version, band)).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > 256 {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

/// Sections computed for a chunk, kept so a reader (the anti-xray exposure test, or the
/// chunk's own delivery after a neighbour prefetched it) can read a packed section instead of
/// evaluating cells. The key carries the dimension so a Nether/End stream cannot collide with
/// an Overworld chunk at the same `(cx, cz)`.
///
/// Each slot is an `Arc<PalettedSection>`, not a 16 KB `Box<[u32; 4096]>`: a typical section
/// holds a 4-8 bit palette (a few hundred bytes to ~4 KB; Direct is 8 KB), and the delivery
/// path can now reuse the very same `Arc` it publishes — no 16 KB copy out of the store and
/// no repack on the way back to the wire. Bounded to the last 32 chunks.
struct SectionStore {
    map: HashMap<(DimId, i32, i32), Vec<Option<std::sync::Arc<PalettedSection>>>>,
    order: std::collections::VecDeque<(DimId, i32, i32)>,
    generation: u64,
}

static SECTION_STORE: std::sync::OnceLock<Mutex<SectionStore>> = std::sync::OnceLock::new();

fn section_store() -> &'static Mutex<SectionStore> {
    SECTION_STORE.get_or_init(|| {
        Mutex::new(SectionStore {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
            generation: current_gen_seq(),
        })
    })
}

/// `(world min_y, section count)` for a dimension. The vanilla windows are the same
/// answers the `dimension_type` declares — Overworld `-64..320` (24 sections), Nether
/// and End `0..256` (16) — and a custom dimension answers whatever it declared, e.g.
/// 12 sections for `min_y: -32, height: 192`.
#[inline]
fn section_window(dim: DimId) -> (i32, usize) {
    (dim.min_y(), dim.sections() as usize)
}

fn section_index_dim(dim: DimId, sy: i32) -> Option<usize> {
    let (min_y, count) = section_window(dim);
    let idx = (sy - min_y) >> 4;
    if (0..count as i32).contains(&idx) {
        Some(idx as usize)
    } else {
        None
    }
}

#[cfg(test)]
fn section_index(sy: i32) -> Option<usize> {
    section_index_dim(DimId::Overworld, sy)
}

/// Pack generated states into the representation the store and the wire share. The
/// predicates must match the delivery loop's (`breg().opacity_of(st) == 15` and
/// `HiddenSet`), otherwise the section's `light_class`/`has_hidden` would disagree with
/// the raw states and the zero-repack fast path would take the wrong branch.
fn pack_section(states: &[u32; 4096]) -> PalettedSection {
    let reg = breg();
    let hidden = HiddenSet::new();
    let is_opaque = |st: u32| reg.is_some_and(|r| r.opacity_of(st) == 15);
    let is_hid = |st: u32| hidden.contains(st);
    PalettedSection::pack(states, &is_opaque, &is_hid)
}

fn publish_section(dim: DimId, cx: i32, cz: i32, sy: i32, sec: std::sync::Arc<PalettedSection>) {
    let Some(idx) = section_index_dim(dim, sy) else {
        return;
    };
    let mut store = section_store().lock().unwrap();
    let seq = current_gen_seq();
    if store.generation != seq {
        store.map.clear();
        store.order.clear();
        store.generation = seq;
    }
    let key = (dim, cx, cz);
    if !store.map.contains_key(&key) {
        let (_, count) = section_window(dim);
        store.map.insert(key, (0..count).map(|_| None).collect());
        store.order.push_back(key);
        while store.order.len() > 32 {
            if let Some(old) = store.order.pop_front() {
                store.map.remove(&old);
            }
        }
    }
    if let Some(sections) = store.map.get_mut(&key) {
        sections[idx] = Some(sec);
    }
}

/// The published section, *without* removing it: the light pass for this chunk and for its
/// neighbours reads the same sections, so consuming one must not empty the store. The LRU
/// bound is what keeps the memory in check. Returns the stored `Arc` itself, so the caller
/// can publish and encode the same section without a 16 KB copy.
fn take_published(dim: DimId, cx: i32, cz: i32, sy: i32) -> Option<std::sync::Arc<PalettedSection>> {
    let idx = section_index_dim(dim, sy)?;
    let store = section_store().lock().unwrap();
    if store.generation != current_gen_seq() {
        return None;
    }
    store.map.get(&(dim, cx, cz))?.get(idx)?.clone()
}

/// Is this section already published? The ring prefetch asks before computing, so a section
/// is computed once no matter how many neighbours want it.
fn has_section(dim: DimId, cx: i32, cz: i32, sy: i32) -> bool {
    let Some(idx) = section_index_dim(dim, sy) else {
        return false;
    };
    let store = section_store().lock().unwrap();
    if store.generation != current_gen_seq() {
        return false;
    }
    store
        .map
        .get(&(dim, cx, cz))
        .is_some_and(|s| s[idx].is_some())
}

/// The published section as a shared handle, for a scan that reads many of its cells.
fn stored_section(dim: DimId, cx: i32, cz: i32, sy: i32) -> Option<std::sync::Arc<PalettedSection>> {
    let idx = section_index_dim(dim, sy)?;
    let store = section_store().lock().unwrap();
    if store.generation != current_gen_seq() {
        return None;
    }
    store.map.get(&(dim, cx, cz))?.get(idx)?.clone()
}

/// The surface band for a chunk in `dim`, if installed and current.
fn band_for(dim: DimId, cx: i32, cz: i32) -> Option<std::sync::Arc<OpBand>> {
    op_bands().lock().unwrap().get((dim, cx, cz))
}

fn stored_state(dim: DimId, x: i32, y: i32, z: i32) -> Option<u32> {
    let idx = section_index_dim(dim, y & !15)?;
    let store = section_store().lock().unwrap();
    if store.generation != current_gen_seq() {
        return None;
    }
    let sections = store.map.get(&(dim, x >> 4, z >> 4))?;
    Some(
        sections[idx].as_ref()?.get(
            (((y & 15) as usize) << 8) | (((z & 15) as usize) << 4) | ((x & 15) as usize),
        ),
    )
}

fn store_patch(dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    let Some(idx) = section_index_dim(dim, y & !15) else {
        return;
    };
    let reg = breg();
    let hidden = HiddenSet::new();
    let is_opaque = |s: u32| reg.is_some_and(|r| r.opacity_of(s) == 15);
    let is_hid = |s: u32| hidden.contains(s);
    let mut store = section_store().lock().unwrap();
    let Some(sections) = store.map.get_mut(&(dim, x >> 4, z >> 4)) else {
        return;
    };
    let Some(sec) = sections[idx].as_mut() else {
        return;
    };
    std::sync::Arc::make_mut(sec).set(
        (((y & 15) as usize) << 8) | (((z & 15) as usize) << 4) | ((x & 15) as usize),
        st,
        &is_opaque,
        &is_hid,
    );
}

fn band_patch_dim(dim: DimId, x: i32, y: i32, z: i32, st: u32) {
    let (cx, cz) = (x >> 4, z >> 4);
    let mut bands = op_bands().lock().unwrap();
    let Some((_, slot)) = bands.map.get_mut(&(dim, cx, cz)) else {
        return;
    };
    let band = std::sync::Arc::make_mut(slot);
    if y < band.y0 || y > band.y1 {
        return;
    }
    band.cells[(((y - band.y0) as usize) << 8) | (((z & 15) as usize) << 4) | (x & 15) as usize] =
        st;
}

fn band_state(x: i32, y: i32, z: i32) -> Option<u32> {
    band_state_dim(DimId::Overworld, x, y, z)
}

fn band_state_dim(dim: DimId, x: i32, y: i32, z: i32) -> Option<u32> {
    if !worldgen::enabled() {
        return None;
    }
    let (cx, cz) = (x >> 4, z >> 4);
    // No version check: the band is write-through (`band_patch` above patches every
    // `set_block_if`), so a band that exists is current for the cells it covers. Asking
    // for the version here would make every leaf write invalidate the chunk whose band the
    // same write has just been applied to.
    //
    // And no build: a reader that misses falls back to `block_at`, which costs ~1.5 µs for
    // this one cell. Building a band costs ~28 ms (it evaluates the surface sections, and
    // for an unpopulated chunk that includes its population), so it only pays when the
    // reader wants thousands of cells from that chunk — which is `chunk_data`, and it
    // installs the band from states it already has (`install_band_rows`).
    let band = op_bands().lock().unwrap().get((dim, cx, cz))?;
    band.get(y, (x & 15) as usize, (z & 15) as usize)
}

/// Opacity of the world block at `(x, y, z)` without one locked terrain evaluation per
/// cell: from the chunk's cached band when the cell is inside it, from `block_at`
/// otherwise (above the band, below it, or before the generator is up).
/// The y band a chunk's light-relevant cells live in: `[min wtop - 18, max wtop + 18]`.
/// Both users stay inside it (the sky floor scan walks down from `surface + 17`, the
/// spill BFS spreads ±15 around a floor), and it is derived from `ChunkCols`, so it
/// costs one hash lookup rather than a terrain pass.
fn band_range(dim: DimId, cx: i32, cz: i32) -> Option<(i32, i32)> {
    let cols = worldgen::chunk_cols_dim(dim, cx, cz)?;
    let (mut lo, mut hi) = (i32::MAX, i32::MIN);
    for lz in 0..16usize {
        for lx in 0..16usize {
            let t = cols.wtop(lx, lz);
            lo = lo.min(t);
            hi = hi.max(t);
        }
    }
    Some((
        (lo - 18).max(dim.min_y()),
        (hi + 18).min(dim.min_y() + dim.height() - 1),
    ))
}

/// Record the opacity rows `chunk_data` has just computed for this section.
///
/// `chunk_data` evaluates the same cells the light band needs (the sections around a
/// chunk's surface) and then throws the states away. Without this, the band was rebuilt
/// from worldgen later — measured at 13.6-15.7 ms per new band (~700 ns/cell) against
/// the ~200 ns/cell the delivery pass pays, i.e. the same work done twice. Installing
/// the rows here makes the band nearly free for a chunk that was just streamed.
fn install_band_rows(dim: DimId, cx: i32, cz: i32, sy: i32, sec: &PalettedSection) {
    if breg().is_none() {
        return;
    }
    let Some((y0, y1)) = band_range(dim, cx, cz) else {
        return;
    };
    if sy + 15 < y0 || sy > y1 {
        return;
    }
    let epoch = chunk_version_dim(dim, cx, cz);
    let mut bands = op_bands().lock().unwrap();
    if bands.get((dim, cx, cz)).is_none() {
        let rows = (y1 - y0 + 1) as usize;
        bands.put(
            (dim, cx, cz),
            epoch,
            std::sync::Arc::new(OpBand {
                y0,
                y1,
                cells: vec![0u32; rows << 8],
            }),
        );
    }
    let Some((_, slot)) = bands.map.get_mut(&(dim, cx, cz)) else {
        return;
    };
    let band = std::sync::Arc::make_mut(slot);
    let ly0 = (band.y0 - sy).max(0) as usize;
    let ly1 = (band.y1 - sy).min(15) as usize;
    if ly0 > ly1 {
        return;
    }
    let count = ly1 - ly0 + 1;
    // `copy_y_slice` fills a uniform section with one `fill` and otherwise unpacks only
    // the rows the band overlaps, so neither shape needs a 4096-cell scan here.
    let mut rows = [0u32; 4096];
    sec.copy_y_slice(ly0, count, &mut rows[..count * 256]);
    for ly in ly0..=ly1 {
        let dst = ((sy + ly as i32 - band.y0) as usize) << 8;
        let src = (ly - ly0) * 256;
        band.cells[dst..dst + 256].copy_from_slice(&rows[src..src + 256]);
    }
}

/// Apply a chunk's overlay edits to its band: the overlay wins over generated state,
/// exactly as `block_at` prefers it. Called once per chunk delivery, after the rows.
fn overlay_band(dim: DimId, cx: i32, cz: i32) {
    if crate::block_registry::breg().is_none() {
        return;
    }
    let mut bands = op_bands().lock().unwrap();
    let Some((_, slot)) = bands.map.get_mut(&(dim, cx, cz)) else {
        return;
    };
    let band = std::sync::Arc::make_mut(slot);
    let (y0, y1) = (band.y0, band.y1);
    // Copy the edits, then apply them: the guard would otherwise be held while the band
    // rows are written, and the tick thread reads `world()` every tick.
    let col: Vec<(u32, u32)> = {
        let w = world().lock().unwrap();
        match w.get(&(dim, cx, cz)) {
            Some(c) => c.iter().map(|(k, v)| (*k, *v)).collect(),
            None => {
                return;
            }
        }
    };
    for (idx, st) in &col {
        let y = (*idx >> 8) as i32 + MIN_Y;
        if y < y0 || y > y1 {
            continue;
        }
        let lx = (*idx & 15) as usize;
        let lz = ((*idx >> 4) & 15) as usize;
        band.cells[(((y - y0) as usize) << 8) | (lz << 4) | lx] = *st;
    }
}

thread_local! {
    /// One chunk-section or band, held for the duration of a scan.
    ///
    /// The light pass reads a column at a time (the sky floor scan walks down, the spill BFS
    /// walks out), so consecutive reads share a chunk *and* a section. Fetching the store entry
    /// and then the band entry per read meant two mutex acquisitions per cell — 1,127 of them
    /// per delivered chunk for the floors alone, measured at 15 µs each. Holding the source
    /// across the scan removes the per-cell locks; a write during the scan patches a *copy*
    /// (`Arc::make_mut`), so a read may see the pre-write value for the length of one scan. The
    /// per-chunk version invalidates the derived floors on that same write, so the next pass
    /// recomputes them.
    static CELL_SOURCE: std::cell::RefCell<
        Option<(u64, (DimId, i32, i32, i32), CellSource)>,
    > = const { std::cell::RefCell::new(None) };
}

#[derive(Clone)]
enum CellSource {
    Section(std::sync::Arc<PalettedSection>),
    Band(std::sync::Arc<OpBand>),
}

/// The section (full height) or band entry covering `(x, y, z)`, with the store's and the
/// band's locks taken at most once per distinct chunk-section. The generator sequence is
/// part of the cache key: a section published by a replaced generator must not answer for
/// the new one (the store itself checks the same sequence on every miss).
fn cell_source(dim: DimId, x: i32, y: i32, z: i32) -> Option<CellSource> {
    // The dimension is part of the key: a Nether and an Overworld query at the same chunk and
    // section must never share a cached source, even back to back on one thread.
    let key = (dim, x >> 4, y >> 4, z >> 4);
    let seq = current_gen_seq();
    if let Some((cached_seq, k, src)) = CELL_SOURCE.with(|c| c.borrow().clone()) {
        if cached_seq == seq && k == key {
            return Some(src);
        }
    }
    let src = stored_section(dim, x >> 4, z >> 4, y & !15)
        .map(CellSource::Section)
        .or_else(|| band_for(dim, x >> 4, z >> 4).map(CellSource::Band))?;
    CELL_SOURCE.with(|c| *c.borrow_mut() = Some((seq, key, src.clone())));
    Some(src)
}

fn opacity_windowed(x: i32, y: i32, z: i32) -> u8 {
    opacity_windowed_dim(DimId::Overworld, x, y, z)
}

fn opacity_windowed_dim(dim: DimId, x: i32, y: i32, z: i32) -> u8 {
    let Some(reg) = breg() else {
        return 0;
    };
    if !worldgen::enabled() {
        return reg.opacity_of(block_at_dim(dim, x, y, z));
    }
    // The published sections first, then the surface band. The band stops 18 blocks below
    // the surface, and the light pass reads the 3x3 ring: a neighbour that has not been
    // delivered yet has no band, and every one of its cells used to fall through to
    // `block_at` (a full worldgen evaluation). Measured: 13,571 such calls per delivered
    // chunk, ~20 ms. The +x neighbour's sections are in the store already (the delivery
    // path's prefetch publishes them), and they cover the full height.
    //
    // A packed section carries enough metadata to answer whole-class reads in O(1):
    // `Empty` is every cell air/void_air/cave_air (opacity 0), `Opaque` is every cell at
    // opacity 15. Only `Mixed` needs a single palette lookup at the cell's index.
    match cell_source(dim, x, y, z) {
        Some(CellSource::Section(sec)) => match sec.light_class() {
            SectionLightClass::Empty => 0,
            SectionLightClass::Opaque => 15,
            SectionLightClass::Mixed => {
                let idx = (((y & 15) as usize) << 8) | (((z & 15) as usize) << 4) | ((x & 15) as usize);
                reg.opacity_of(sec.get(idx))
            }
        },
        Some(CellSource::Band(b)) => b
            .get(y, (x & 15) as usize, (z & 15) as usize)
            .map_or_else(|| reg.opacity_of(block_at_dim(dim, x, y, z)), |st| reg.opacity_of(st)),
        None => reg.opacity_of(block_at_dim(dim, x, y, z)),
    }
}

/// Per-chunk sky floors, memoized.
///
/// `sky_floors` walks 256 columns and up to 24 cells each, and every `opacity` call is
/// a `block_at` (one world lock). Measured under `MC_SPINE_CHUNK_PROFILE=1`: 32-62 ms
/// per delivered chunk, in **10 calls** — the centre chunk plus the whole 3x3 window,
/// each of which is the centre of some other chunk's delivery.
///
/// Invalidation: a floor is a function of the blocks in its chunk, so an entry is used
/// only while that chunk's write version has not moved (`bump_world_epoch`).
///
/// A hash index plus insertion order, exactly like `BandCache`: the first version scanned a
/// `VecDeque` per lookup, and this cache is asked ten times per delivered chunk with a 1 KiB
/// entry to find, so the scan was the whole cost of a hit. `order` exists only for the bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SkyFloorsKey(DimId, i32, i32);

impl From<(i32, i32)> for SkyFloorsKey {
    fn from((cx, cz): (i32, i32)) -> Self {
        Self(DimId::Overworld, cx, cz)
    }
}

impl From<(DimId, i32, i32)> for SkyFloorsKey {
    fn from((dim, cx, cz): (DimId, i32, i32)) -> Self {
        Self(dim, cx, cz)
    }
}

struct SkyFloorsCache {
    map: HashMap<SkyFloorsKey, (u64, [i32; 256])>,
    order: std::collections::VecDeque<SkyFloorsKey>,
}

impl SkyFloorsCache {
    /// Entries retained; a moving viewer asks for a chunk's floors while it is in reach and
    /// never again, so eviction order is insertion order.
    const CAP: usize = 256;

    fn len(&self) -> usize {
        self.map.len()
    }

    /// The floors for `key`, when the entry was computed at the chunk's current version.
    fn get(&self, key: impl Into<SkyFloorsKey>, epoch: u64) -> Option<[i32; 256]> {
        match self.map.get(&key.into()) {
            Some((entry_epoch, floors)) if *entry_epoch == epoch => Some(*floors),
            _ => None,
        }
    }

    /// Store (or refresh) an entry, evicting the oldest when the bound is passed. A key that is
    /// already present is updated in place: it keeps its position in `order`, so a refresh is
    /// not an insertion and the bound cannot be moved by one chunk being asked for repeatedly.
    fn put(&mut self, key: impl Into<SkyFloorsKey>, epoch: u64, floors: [i32; 256]) {
        let key = key.into();
        if let Some(slot) = self.map.get_mut(&key) {
            slot.0 = epoch;
            slot.1 = floors;
            return;
        }
        if self.len() >= Self::CAP {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.map.insert(key, (epoch, floors));
        self.order.push_back(key);
    }
}

static SKY_FLOORS: OnceLock<Mutex<SkyFloorsCache>> = OnceLock::new();

fn sky_floors_cache() -> &'static Mutex<SkyFloorsCache> {
    SKY_FLOORS.get_or_init(|| {
        Mutex::new(SkyFloorsCache {
            map: HashMap::new(),
            order: std::collections::VecDeque::new(),
        })
    })
}

/// A per-chunk write version. Cached per-chunk data (the state band, the sky floors) is
/// valid only while this has not moved for *that* chunk.
///
/// A world-wide epoch was wrong here for a live server: leaves, crops and fluids write
/// every tick, so one write anywhere would invalidate every cached band and the next
/// light pass would rebuild the whole 3x3 (14 ms each). A write only changes its own
/// chunk's cells, and each cache entry describes one chunk, so the version is per chunk.
static CHUNK_VERSIONS: OnceLock<Mutex<HashMap<(DimId, i32, i32), u64>>> = OnceLock::new();
static CHUNK_VERSION_SEQ: AtomicU64 = AtomicU64::new(1);

fn chunk_versions() -> &'static Mutex<HashMap<(DimId, i32, i32), u64>> {
    CHUNK_VERSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn chunk_version_dim(dim: DimId, cx: i32, cz: i32) -> u64 {
    chunk_versions()
        .lock()
        .unwrap()
        .get(&(dim, cx, cz))
        .copied()
        .unwrap_or(0)
}

/// Call after any write into `world()` at `(x, z)`: that chunk's caches are now suspect.
fn bump_world_epoch(x: i32, z: i32) {
    bump_world_epoch_dim(DimId::Overworld, x, z);
}

fn bump_world_epoch_dim(dim: DimId, x: i32, z: i32) {
    let seq = CHUNK_VERSION_SEQ.fetch_add(1, Ordering::Relaxed);
    chunk_versions()
        .lock()
        .unwrap()
        .insert((dim, x >> 4, z >> 4), seq);
    if chunkprof::enabled() {
        chunkprof::EPOCH_BUMPS.fetch_add(1, Ordering::Relaxed);
    }
}

fn sky_floors(cx: i32, cz: i32, opacity: &impl Fn(i32, i32, i32) -> u8) -> [i32; 256] {
    sky_floors_dim(DimId::Overworld, cx, cz, opacity)
}

fn sky_floors_dim(
    dim: DimId,
    cx: i32,
    cz: i32,
    opacity: &impl Fn(i32, i32, i32) -> u8,
) -> [i32; 256] {
    let __t = chunkprof::enabled().then(std::time::Instant::now);
    let epoch = chunk_version_dim(dim, cx, cz);
    if let Some(floors) = sky_floors_cache().lock().unwrap().get((dim, cx, cz), epoch) {
        if let Some(t) = __t {
            chunkprof::add(&chunkprof::FLOORS, t);
        }
        return floors;
    }
    if __t.is_some() {
        chunkprof::FLOORS_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    // Start each column just above the highest cell that can block light, instead of a fixed
    // 17 above the terrain top: the scan below walks down until it finds one, and with the
    // old start that walk was ~17 air cells of the 24-step budget, ~6,000 `opacity` calls
    // per call to this function (measured 13.5 ms each, 23 ms per delivered chunk).
    //
    // The bound is the same basis the live heightmaps use: the chunk's own `wtop` (the
    // topmost solid-or-fluid of the *generated* column) raised by whatever the feature layer
    // wrote above it, which is where trees live. Both are reads of state that already exists,
    // so this costs one pass over the chunk's columns rather than a per-cell scan.
    let __ts = chunkprof::enabled().then(std::time::Instant::now);
    let starts: [i32; 256] = {
        let cols = worldgen::chunk_cols_dim(dim, cx, cz);
        let feature = worldgen::with_gen_dim(dim, |g| {
            let mut v = [i32::MIN; 256];
            for lz in 0..16usize {
                for lx in 0..16usize {
                    let (x, z) = (cx * 16 + lx as i32, cz * 16 + lz as i32);
                    v[(lz << 4) | lx] = g.feature_height(x, z, 0);
                }
            }
            v
        })
        .unwrap_or([i32::MIN; 256]);
        let mut starts = [0i32; 256];
        for lz in 0..16usize {
            for lx in 0..16usize {
                let i = (lz << 4) | lx;
                let base = cols
                    .as_ref()
                    .map(|c| c.wtop(lx, lz) as i32)
                    .unwrap_or_else(|| height_at_dim(dim, cx * 16 + lx as i32, cz * 16 + lz as i32));
                starts[i] = base.max(feature[i]) + 1;
            }
        }
        starts
    };
    let mut floors = [0i32; 256];
    for lz in 0..16usize {
        for lx in 0..16usize {
            let i = (lz << 4) | lx;
            let (x, z) = (cx * 16 + lx as i32, cz * 16 + lz as i32);
            let mut y = starts[i].min(dim.min_y() + dim.height() - 1);
            let mut steps = 0;
            while steps < 24 {
                if chunkprof::enabled() {
                    chunkprof::FLOORS_CELLS.fetch_add(1, Ordering::Relaxed);
                }
                if opacity(x, y, z) > 0 {
                    break;
                }
                y -= 1;
                steps += 1;
            }
            floors[i] = y;
        }
    }
    sky_floors_cache()
        .lock()
        .unwrap()
        .put((dim, cx, cz), epoch, floors);
    if let Some(t) = __t {
        chunkprof::add(&chunkprof::FLOORS, t);
    }
    floors
}

/// Sky light for a chunk, from the world: the per-column value, raised where light has arrived
/// sideways.
fn sky_light_sections_world(cx: i32, cz: i32) -> Vec<[u8; 2048]> {
    sky_light_sections_dim(DimId::Overworld, cx, cz)
}

/// The same sky-light composition for any dimension: its own settled light, its own
/// light-section count, its own floors and opacity reads.
fn sky_light_sections_dim(dim: DimId, cx: i32, cz: i32) -> Vec<[u8; 2048]> {
    if let Some(light) = settled_live_light(dim, cx, cz) {
        return light.sky.clone();
    }
    if crate::block_registry::breg().is_none() {
        return vec![[0u8; 2048]; dim.light_sections()];
    }
    let op = |x: i32, y: i32, z: i32| opacity_windowed_dim(dim, x, y, z);
    let floors = sky_floors_dim(dim, cx, cz, &op);
    // Floors for the 3x3 window: a margin cell (a neighbour chunk) must be tested against its own
    // column, not wrapped onto the centre chunk's, or the BFS routes through cells that are not dark.
    let mut f9 = [[0i32; 256]; 9];
    for dx in -1..=1i32 {
        for dz in -1..=1i32 {
            f9[((dx + 1) * 3 + (dz + 1)) as usize] = sky_floors_dim(dim, cx + dx, cz + dz, &op);
        }
    }
    let floor_of = |x: i32, z: i32| -> i32 {
        let dx = (x >> 4) - cx + 1;
        let dz = (z >> 4) - cz + 1;
        if !(0..=2).contains(&dx) || !(0..=2).contains(&dz) {
            return i32::MIN;
        }
        f9[(dx * 3 + dz) as usize][(((z & 15) as usize) << 4) | (x & 15) as usize]
    };
    // The spill raises only the cells the column rule left dark, so it composes onto the packed
    // columns instead of replacing them; a map is never built.
    let mut out = sky_light_sections_from_dim(dim, cx, cz, &floors);
    sky_spill_apply(dim, cx, cz, floor_of, op, &mut out);
    out
}

#[cfg(test)]
/// The composition the tests drive: a stored spill map merged into the packed columns. Kept as
/// the independent statement of what the dense grid must produce — `light_pack_tests` hands it
/// levels the producer cannot make (negative coordinates, out-of-range y) and compares every
/// nibble against a formula written from the layout alone.
fn sky_light_sections_with_spill(
    cx: i32,
    cz: i32,
    floors: &[i32; 256],
    spill: &HashMap<(i32, i32, i32), u8>,
) -> Vec<[u8; 2048]> {
    // Start with the column packer, then visit only the actual spill entries.
    // A seed makes spill nonempty even when it raises no dark cell; probing the
    // map at every voxel needlessly defeated the uniform-section fast paths.
    let mut out = sky_light_sections_from(cx, cz, floors);
    for (&(x, y, z), &level) in spill {
        raise_sky_nibble(DimId::Overworld, &mut out, cx, cz, x, y, z, level);
    }
    out
}

/// One section's 2048 bytes from a level function, in the client's layout: for each y,
/// each z, eight bytes carrying the sixteen x values as nibbles. Both light kinds need this
/// and a wrong index lights the wrong column with nothing else to notice, so it has one
/// owner.
fn pack_light_section(sy: i32, level: impl Fn(usize, i32, usize) -> u8) -> [u8; 2048] {
    let mut arr = [0u8; 2048];
    for y in 0..16usize {
        let wy = sy + y as i32;
        for lz in 0..16usize {
            for half in 0..8usize {
                let lx = half * 2;
                let lo = level(lx, wy, lz) & 0x0f;
                let hi = level(lx + 1, wy, lz) & 0x0f;
                arr[y * 128 + lz * 8 + half] = lo | (hi << 4);
            }
        }
    }
    arr
}

/// One section's sky light, written a column at a time. The generic packer reads the floor
/// twice per byte, 4096 reads a section; a column has one floor per x and writes its sixteen
/// bytes together, so the reads drop to 256 and there is no closure to call. This is where the
/// cost was: the packing, not the block lookups.
fn pack_sky_section(sy: i32, floors: &[i32; 256]) -> [u8; 2048] {
    let mut arr = [0u8; 2048];
    for lz in 0..16usize {
        for half in 0..8usize {
            let lx = half * 2;
            let f0 = floors[(lz << 4) | lx];
            let f1 = floors[(lz << 4) | (lx + 1)];
            for y in 0..16usize {
                let wy = sy + y as i32;
                let lo = if wy > f0 { 0x0f } else { 0x00 };
                let hi = if wy > f1 { 0xf0 } else { 0x00 };
                arr[y * 128 + lz * 8 + half] = lo | hi;
            }
        }
    }
    arr
}

#[cfg(test)]
fn sky_light_sections_from(cx: i32, cz: i32, floors: &[i32; 256]) -> Vec<[u8; 2048]> {
    sky_light_sections_from_dim(DimId::Overworld, cx, cz, floors)
}

fn sky_light_sections_from_dim(
    dim: DimId,
    cx: i32,
    cz: i32,
    floors: &[i32; 256],
) -> Vec<[u8; 2048]> {
    let _ = (cx, cz);
    let (mut fmin, mut fmax) = (i32::MAX, i32::MIN);
    for f in floors {
        fmin = fmin.min(*f);
        fmax = fmax.max(*f);
    }
    let light_min_y = dim.light_min_y();
    let mut out = Vec::with_capacity(dim.light_sections());
    for sec in 0..dim.light_sections() as i32 {
        let sy = sec * 16 + light_min_y;
        let mut arr = [0u8; 2048];
        if fmax < sy {
            // Every floor is below this section, so the whole section is above its own
            // floor and open to the sky.
            arr = [0xffu8; 2048];
        } else if fmin >= sy + 15 {
            // Every floor is at or above the section's top, so the whole section sits below
            // the light and stays dark - which all-zero already is.
        } else if fmin == fmax {
            // Every column shares this floor, so the section is a run of dark rows and then a
            // run of lit ones. Relief makes this rare, so it is a branch for the flat case and
            // not the expected path.
            for y in 0..16usize {
                if sy + y as i32 > fmin {
                    for i in 0..128 {
                        arr[y * 128 + i] = 0xff;
                    }
                }
            }
        } else {
            arr = pack_sky_section(sy, floors);
        }
        out.push(arr);
    }
    out
}

/// One chunk's payload for one dimension.
///
/// The dimension is threaded from the caller that decided whose chunk this is, not read from
/// whatever `init` installed: a Nether section is built by the Nether's generator, and the
/// Overworld arm passes [`DimId::Overworld`] and is byte-for-byte what it was before the
/// per-dimension registry existed.
#[cfg(test)]
fn chunk_data(cx: i32, cz: i32, dim: DimId, gen: &mut GenSources) -> Vec<u8> {
    chunk_data_opt(cx, cz, dim, gen, chunk_prefetch_enabled())
}

/// On by default. `MC_SPINE_NO_PREFETCH_NEIGHBOUR=1` turns it off (the digest guard
/// passes the setting explicitly, so it can compare both without the environment).
fn chunk_prefetch_enabled() -> bool {
    std::env::var("MC_SPINE_NO_PREFETCH_NEIGHBOUR").is_err()
}

#[cfg(test)]
fn chunk_data_opt(
    cx: i32,
    cz: i32,
    dim: DimId,
    gen: &mut GenSources,
    prefetch: bool,
) -> Vec<u8> {
    chunk_data_lane(cx, cz, dim, gen, prefetch, false)
}

/// The chunk's own occlusion ceiling, from the lowest surface column in it.
/// Every section whose top is at or below it is sent as solid host rock in the
/// `occluded` lane, so a surface x-ray client cannot see caves, mineshafts,
/// trial chambers or strongholds at all. `hmin - 16` keeps the ceiling under a
/// deep ocean floor or an open ravine mouth, so an honest player looking down
/// one never sees fake stone sealing the opening; the `.min(32)` caps it under
/// ordinary plains and hills.
fn occlusion_ceil_from_hmin(hmin: i32) -> i32 {
    ((hmin - 16).min(32)) & !15
}

/// The occlusion ceiling for one chunk in a skylight dimension, `None` where
/// section occlusion does not apply (Nether, End, or a `has_skylight = false`
/// custom dimension: open ceiling/void cavities are normal there).
fn underground_occlude_ceil(dim: DimId, cx: i32, cz: i32) -> Option<i32> {
    if !dim.has_skylight() {
        return None;
    }
    let mut hmin = i32::MAX;
    for lz in 0..16 {
        for lx in 0..16 {
            hmin = hmin.min(height_at_dim(dim, cx * 16 + lx, cz * 16 + lz));
        }
    }
    Some(occlusion_ceil_from_hmin(hmin))
}

fn chunk_data_lane(
    cx: i32,
    cz: i32,
    dim: DimId,
    gen: &mut GenSources,
    prefetch: bool,
    occluded: bool,
) -> Vec<u8> {
    let mut data = Vec::new();
    let reg = crate::block_registry::breg();
    let mut gen_sources_here: Vec<((i32, i32, i32), u8)> = Vec::new();
    // One short lock for this chunk's edits, then work off the copy. The section loop
    // below used to hold `world()` while it encoded 4096 cells per section, and the tick
    // thread queues behind it: measured, the pressure-plate step waited 17.6 ms per tick
    // on 16 `world()` reads, which is the whole tick body. The copy is the chunk's own
    // edit map (a handful of entries, empty on a fresh chunk).
    let __to = chunkprof::enabled().then(std::time::Instant::now);
    // This chunk belongs to `dim`, so the edits that apply to it are `dim`'s. The band and the
    // section store this function then fills (`install_band_rows`, `publish_section`) are still
    // keyed by `(cx, cz)` alone, which is why every caller passes the Overworld today; S3 is what
    // gives those caches a dimension.
    let overlay: HashMap<u32, u32> = {
        let w = world().lock().unwrap();
        w.get(&(dim, cx, cz)).cloned().unwrap_or_default()
    };
    if let Some(t) = __to {
        chunkprof::add(&chunkprof::OVERLAY_APPLY, t);
    }
    let overlay_empty = overlay.is_empty();
    // The block section window is this dimension's, not the Overworld's hard-coded -64/384.
    // Overworld keeps its 24 sections from -64; the Nether and the End get 16 from 0.
    let min_y = dim.min_y();
    let sections = dim.sections();
    let top_y = min_y + dim.height();
    let mut hs = [0i32; 256];
    let (mut hmin, mut hmax) = (i32::MAX, i32::MIN);
    for lz in 0..16 {
        for lx in 0..16 {
            let h = height_at_dim(dim, cx * 16 + lx, cz * 16 + lz);
            hs[((lz << 4) | lx) as usize] = h;
            hmin = hmin.min(h);
            hmax = hmax.max(h);
        }
    }
    // Occluded lane: sections fully below the ceiling are sent as solid host rock
    // instead of terrain, so a surface x-ray client sees no caves at all. The same
    // `hmin` the terrain fast path uses decides the ceiling; skylight dimensions
    // only — Nether/End keep their open cavities. Overlay edits cannot unmask a
    // section: the whole point is that buried builds stay invisible too.
    let occlude_ceil_y = if occluded && dim.has_skylight() {
        Some(occlusion_ceil_from_hmin(hmin))
    } else {
        None
    };
    let (mask_deep, mask_shallow) = if occlude_ceil_y.is_some() {
        (
            name_to_state("deepslate").unwrap_or(1),
            name_to_state("stone").unwrap_or(1),
        )
    } else {
        (0, 0)
    };
    // The anti-xray exposure test reads ±1 in y as well as x/z, and its vertical
    // neighbours are the adjacent sections. Carrying the next section's terrain forward
    // (and the previous one back) means those reads are array reads too: measured, the
    // 621 reads that still fell through to `base_state` cost 36.5 ms of the encode loop's
    // 39 ms (58 µs each, because a cold cell re-runs the material rules).
    // Prefetch the +x neighbour's sections for the same y band, publish them, and let that
    // neighbour's own delivery consume them from the store instead of recomputing. It
    // answers the exposure test's deep horizontal reads, which are the encode loop's cost
    // (measured: 482 `base_state` calls at 75 µs = 37 ms of the loop's 42).
    //
    // On by default. It does populate the neighbour a step before its arrival, and the order
    // chunks are populated in *is* an input to the feature layer, so that had to be checked
    // rather than assumed: `chunk_delivery_digest` compares terrain-state digests with this
    // setting on and off over 2 seeds x 3 sweep directions (60 sweeps) and finds them
    // identical, and it asserts that stays true. `MC_SPINE_NO_PREFETCH_NEIGHBOUR=1` reverts.
    // Packed sections, not raw 16 KB arrays: the store hands out the very same `Arc` the
    // encode step writes to the wire, so publishing and reading a section costs neither a
    // copy nor a repack.
    let mut carried_next: Option<std::sync::Arc<PalettedSection>> = None;
    let mut prev_base: Option<std::sync::Arc<PalettedSection>> = None;
    for sec in 0..sections {
        let sy = sec * 16 + min_y;
        let __tb = chunkprof::enabled().then(std::time::Instant::now);
        let biomes = worldgen::biome_section_dim(dim, cx, sy, cz)
            .unwrap_or_else(|| Box::new([39i64; 64])); // plains
        if let Some(t) = __tb {
            chunkprof::add(&chunkprof::BIOMES, t);
        }
        // Occluded lane: this section's whole column band is below the ceiling, so
        // it reaches the wire as one solid host-rock palette. No terrain pass, no
        // exposure reads, and no generated emitters — recording one would let cave
        // lava/torch glow leak through `gen_sources_here` into the light arrays.
        if occlude_ceil_y.is_some_and(|ceil| sy + 16 <= ceil) {
            PalettedSection::single(if sy < 0 { mask_deep } else { mask_shallow }, true, false)
                .write_wire(&biomes, &mut data);
            // A masked section has no real base to carry: clearing both keeps the
            // next real section's vertical exposure reads on actual terrain (via
            // the band/overlay/base_state fallbacks) instead of a stale neighbor.
            prev_base = None;
            carried_next = None;
            continue;
        }
        // uniform fast path only when overlay empty AND terrain can't reach the section
        let wg = worldgen::enabled();
        let __tf = chunkprof::enabled().then(std::time::Instant::now);
        if overlay_empty && ((!wg && sy + 15 < hmin - 4) || sy > hmax + 8) {
            if __tf.is_some() {
                chunkprof::add(&chunkprof::FASTPATH, __tf.unwrap());
                chunkprof::FASTPATH_N.fetch_add(1, Ordering::Relaxed);
            }
            let st = if sy + 15 < hmin - 4 { 1 } else { 0 };
            // A uniform section's wire is just its header; `single` avoids both the
            // 4096-cell stack array and the 4096-cell scan the legacy encoder would do.
            PalettedSection::single(st, false, false).write_wire(&biomes, &mut data);
        } else {
            let mut arr = [0u32; 4096];
            // One terrain pass for the section: the previous shape took the worldgen
            // lock up to five times per block (feature lookup, columns, density,
            // aquifer, deep rules) and could compute the generated state twice when
            // an overlay edit covered the cell.
            let __ts = chunkprof::enabled().then(std::time::Instant::now);
            if __ts.is_some() {
                chunkprof::SECTION_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let base = match carried_next.take() {
                Some(b) => Some(b),
                None => match take_published(dim, cx, cz, sy) {
                    Some(b) => Some(b),
                    None => {
                        if chunkprof::enabled() {
                            chunkprof::TERRAIN_N.fetch_add(1, Ordering::Relaxed);
                        }
                        // Pack once at the source: the same Arc is published, handed to
                        // the band, and written straight to the wire below.
                        worldgen::terrain_section_dim(dim, cx, sy, cz, feature::populate)
                            .map(|rows| std::sync::Arc::new(pack_section(&rows)))
                    }
                },
            };
            if prefetch {
                if let Some(sec) = &base {
                    publish_section(dim, cx, cz, sy, sec.clone());
                    // Only +x. The exposure test also reads +z and the diagonal, and
                    // prefetching those too (measured) takes encode from 29 to 12 ms but
                    // raises the window's CPU from 29.6 to 32.1 s: the diagonal's sections
                    // are often computed for a chunk that is not delivered next. The +x
                    // neighbour is the one a sweep is about to deliver, so only it is
                    // worth computing early.
                    // +x only. Publishing the whole 3x3 ring answers the light pass's
                    // ring reads (light 34.9 -> 14.8 ms, `block_at` 14,012 -> 2,991) but
                    // costs far more in terrain: measured 19 terrain passes per chunk
                    // against 10, because the ring includes rows and diagonals whose own
                    // deliveries are not next. The section phase went 33.4 -> 106.2 ms and
                    // the window's CPU 29.3 -> 31.0 s, so it is a net loss and only +x is
                    // prefetched.
                    if !has_section(dim, cx + 1, cz, sy) {
                        if chunkprof::enabled() {
                            chunkprof::TERRAIN_N.fetch_add(1, Ordering::Relaxed);
                        }
                        if let Some(nbr) =
                            worldgen::terrain_section_dim(dim, cx + 1, sy, cz, feature::populate)
                        {
                            publish_section(
                                dim,
                                cx + 1,
                                cz,
                                sy,
                                std::sync::Arc::new(pack_section(&nbr)),
                            );
                        }
                    }
                }
            }
            // The section above, computed now so this section's reads have it and the next
            // iteration does not pay for it twice.
            let next_base = if sy + 16 < top_y {
                // The +x neighbour's delivery published this one a step ago in a sweep, so
                // ask the store before computing it again.
                match take_published(dim, cx, cz, sy + 16) {
                    Some(b) => Some(b),
                    None => {
                        if chunkprof::enabled() {
                            chunkprof::TERRAIN_N.fetch_add(1, Ordering::Relaxed);
                        }
                        worldgen::terrain_section_dim(dim, cx, sy + 16, cz, feature::populate)
                            .map(|rows| std::sync::Arc::new(pack_section(&rows)))
                    }
                }
            } else {
                None
            };
            if let Some(t) = __ts {
                chunkprof::add(&chunkprof::SECTION, t);
            }
            // Hand the states the light pass needs to its band, instead of letting it
            // re-evaluate them through `block_state_at` later.
            if let Some(sec) = &base {
                let __ti = chunkprof::enabled().then(std::time::Instant::now);
                install_band_rows(dim, cx, cz, sy, sec);
                if let Some(t) = __ti {
                    chunkprof::add(&chunkprof::BAND_INSTALL, t);
                }
            }
            let __te = chunkprof::enabled().then(std::time::Instant::now);
            // A clean, hidden-free section packed by `pack_section` has exactly the palette
            // `section_encode` would choose. Its wire can be written verbatim: no 4096-cell
            // scan, no hidden loop, no second palette build. An overlay or a `set()` patch
            // (which leaves stale palette entries behind) forces the legacy path below.
            let zero_repack = overlay_empty
                && base.as_ref().is_some_and(|s| !s.has_hidden() && s.is_clean_wire());
            let has_base = base.is_some();
            if zero_repack {
                let sec = base.as_ref().expect("zero_repack requires a section");
                if let Some(reg) = reg {
                    // `any_palette_entry` avoids a 4096-cell scan in the vast majority of
                    // sections. When emitters exist, keep the exact (ly, lz, lx) order so
                    // `GenSources` (and therefore block light) is unchanged.
                    if let Some(st) = sec.uniform_state() {
                        // M3 P2c: a `Single` section answers the emitter scan in O(1):
                        // one `luminance_of` call replaces the per-cell `sec.get` +
                        // `luminance_of` pair, and the whole 4,096-cell slice pushes its
                        // emitters together when the light is non-zero. Identical to the
                        // cell-by-cell path for uniform sections
                        // (`p2_c_uniform_sections_emitters_match_cell_by_cell`).
                        let lum = reg.luminance_of(st);
                        if lum > 0 {
                            for ly in 0..16i32 {
                                for lz in 0..16usize {
                                    for lx in 0..16usize {
                                        gen_sources_here.push((
                                            (
                                                cx * 16 + lx as i32,
                                                sy + ly,
                                                cz * 16 + lz as i32,
                                            ),
                                            lum,
                                        ));
                                    }
                                }
                            }
                        }
                    } else if sec.any_palette_entry(|st| reg.luminance_of(st) > 0) {
                        for ly in 0..16usize {
                            for lz in 0..16usize {
                                for lx in 0..16usize {
                                    let gen = sec.get((ly << 8) | (lz << 4) | lx);
                                    let lum = reg.luminance_of(gen);
                                    if lum > 0 {
                                        gen_sources_here.push((
                                            (
                                                cx * 16 + lx as i32,
                                                sy + ly as i32,
                                                cz * 16 + lz as i32,
                                            ),
                                            lum,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                if let Some(sec) = &base {
                    // One fast unpack into the stack array: the 4096-cell loop and the
                    // `gen` reads below are then L1 array reads, not per-cell palette
                    // lookups. `arr` is fully overwritten by the loop that follows.
                    sec.unpack_into(&mut arr);
                }
                {
                // Neighbour reads (the anti-xray exposure test) go through a per-call
                // lock instead of holding `world()` across all 4096 cells: the hold is
                // what the tick thread queues behind, and `base_state` is called with no
                // lock held at all.
                // The anti-xray exposure test is the encode loop's cost: it reads six
                // neighbours per hidden cell, and 6,795 of its 8,715 reads per chunk fell
                // through to a full worldgen evaluation (`base_state`, ~5 µs each = 34 ms
                // of the 45 ms). Almost all of those cells are ones this section's own
                // `base` array already holds, so answer from it: same chunk and section ->
                // the array (after the overlay, which wins), else the band (the surface
                // window), else the overlay, else worldgen.
                let mut read = |wx: i32, wy: i32, wz: i32| -> u32 {
                    if chunkprof::enabled() {
                        chunkprof::EXPOSURE_READS
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    let __t = chunkprof::enabled().then(std::time::Instant::now);
                    if (wx >> 4) == cx && (wz >> 4) == cz {
                        if !overlay_empty {
                            if let Some(&st) = overlay.get(&local_idx(wx, wy, wz)) {
                                return st;
                            }
                        }
                        let lx = (wx & 15) as usize;
                        let lz = (wz & 15) as usize;
                        let section_of =
                            |sec: &Option<std::sync::Arc<PalettedSection>>, dy: i32| -> Option<u32> {
                                let sec = sec.as_ref()?;
                                let idx = ((dy as usize) << 8) | (lz << 4) | lx;
                                Some(sec.get(idx))
                            };
                        if wy >= sy && wy < sy + 16 {
                            if let Some(v) = section_of(&base, wy - sy) {
                                if let Some(t) = __t {
                                    chunkprof::add(&chunkprof::READ_NS, t);
                                }
                                return v;
                            }
                        } else if wy == sy - 1 {
                            if let Some(v) = section_of(&prev_base, 15) {
                                if let Some(t) = __t {
                                    chunkprof::add(&chunkprof::READ_NS, t);
                                }
                                return v;
                            }
                        } else if wy == sy + 16 {
                            if let Some(v) = section_of(&next_base, 0) {
                                if let Some(t) = __t {
                                    chunkprof::add(&chunkprof::READ_NS, t);
                                }
                                return v;
                            }
                        }
                    }
                    let __tb = chunkprof::enabled().then(std::time::Instant::now);
                    if __tb.is_some() {
                        chunkprof::READ_BAND_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    if let Some(st) = stored_state(dim, wx, wy, wz) {
                        if let Some(t) = __t {
                            chunkprof::add(&chunkprof::READ_NS, t);
                        }
                        return st;
                    }
                    let band = band_state_dim(dim, wx, wy, wz);
                    if let Some(t) = __tb {
                        chunkprof::add(&chunkprof::BAND_LOOKUP_NS, t);
                    }
                    let overlay_hit = if band.is_none() {
                        let __to = chunkprof::enabled().then(std::time::Instant::now);
                        let v = overlay_block_dim(dim, wx, wy, wz);
                        if let Some(t) = __to {
                            chunkprof::add(&chunkprof::OVERLAY_READ_NS, t);
                        }
                        v
                    } else {
                        None
                    };
                    let out = match band.or(overlay_hit) {
                        Some(st) => st,
                        None => {
                            if chunkprof::enabled() {
                                chunkprof::EXPOSURE_READ_BASE
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            if chunkprof::enabled() {
                                chunkprof::EXPOSURE_READS
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            let __tbs = chunkprof::enabled().then(std::time::Instant::now);
                            if __tbs.is_some() {
                                chunkprof::BASE_STATE_N
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            let st = base_state_dim(dim, wx, wy, wz);
                            if let Some(t) = __tbs {
                                chunkprof::add(&chunkprof::BASE_STATE_NS, t);
                            }
                            st
                        }
                    };
                    if let Some(t) = __tb {
                        chunkprof::add(&chunkprof::READ_BAND_NS, t);
                    }
                    if let Some(t) = __t {
                        chunkprof::add(&chunkprof::READ_NS, t);
                    }
                    out
                };
                let hidden = HiddenSet::new();
                for ly in 0..16 {
                    for lz in 0..16 {
                        for lx in 0..16 {
                            let wx = cx * 16 + lx as i32;
                            let wz = cz * 16 + lz as i32;
                            let wy = sy + ly as i32;
                            // Every cell of this loop is in this chunk, so the chunk's
                            // own copy of its edits answers it.
                            let ov = if overlay_empty {
                                None
                            } else {
                                overlay.get(&local_idx(wx, wy, wz)).copied()
                            };
                            let gen = if has_base {
                                arr[(ly << 8) | (lz << 4) | lx]
                            } else {
                                base_state_dim(dim, wx, wy, wz)
                            };
                            let st = ov.unwrap_or(gen);
                            // Record the generated emitter even when an overlay edit covers it:
                            // removing that edit re-exposes it, and the live-overlay filter in
                            // `block_light_sections` suppresses it while the edit remains.
                            if let Some(reg) = reg {
                                let lum = reg.luminance_of(gen);
                                if lum > 0 {
                                    gen_sources_here.push(((wx, wy, wz), lum));
                                }
                            }
                            // anti-xray + palette compression: buried hidden blocks sent as stone
                            let mut hidden_cell = hidden.contains(st);
                            if chunkprof::enabled() && hidden_cell {
                                chunkprof::HIDDEN_N
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            if hidden_cell {
                                for (dx, dy, dz) in [
                                    (1, 0, 0),
                                    (-1, 0, 0),
                                    (0, 1, 0),
                                    (0, -1, 0),
                                    (0, 0, 1),
                                    (0, 0, -1),
                                ] {
                                    if exposes(read(wx + dx, wy + dy, wz + dz)) {
                                        hidden_cell = false;
                                        break;
                                    }
                                }
                            }
                            let vis = if hidden_cell {
                                hidden_filler_with(&mut read, dim, wx, wy, wz, hs[(lz << 4) | lx])
                            } else {
                                st
                            };
                            arr[(ly << 8) | (lz << 4) | lx] = vis;
                        }
                    }
                }
                }
            }
            if let Some(t) = __te {
                chunkprof::add(&chunkprof::ENCODE, t);
            }
            let __tl = chunkprof::enabled().then(std::time::Instant::now);
            if zero_repack {
                // Byte-identical to `section_encode_into(&arr, &biomes)` for the states
                // `pack_section` was given, but written straight from the stored palette.
                base.as_ref()
                    .expect("zero_repack requires a section")
                    .write_wire(&biomes, &mut data);
            } else {
                section_encode_into(&mut data, &arr, &biomes);
            }
            if let Some(t) = __tl {
                chunkprof::add(&chunkprof::ENC_SECTION, t);
            }
            prev_base = base;
            carried_next = next_base;
        }
    }
    gen.insert((cx, cz), gen_sources_here);
    data
}

fn nbt_str(v: &mut Vec<u8>, s: &str) {
    // Java writeUTF encodes NUL as C0 80 and astral scalars as two three-byte
    // UTF-16 surrogates. Keep our bounded-prefix policy for server-authored text
    // (not Java's overflow exception), measured in encoded bytes, never half a scalar.
    let start = v.len();
    v.extend_from_slice(&[0, 0]);
    for ch in s.chars() {
        let size = match ch as u32 {
            0 => 2,
            1..=0x7f => 1,
            0x80..=0x7ff => 2,
            0x800..=0xffff => 3,
            _ => 6,
        };
        if v.len() - start - 2 + size > u16::MAX as usize {
            break;
        }
        for &unit in ch.encode_utf16(&mut [0; 2]).iter() {
            match unit {
                1..=0x7f => v.push(unit as u8),
                0..=0x7ff => {
                    v.push(0xc0 | (unit >> 6) as u8);
                    v.push(0x80 | (unit & 0x3f) as u8);
                }
                _ => {
                    v.push(0xe0 | (unit >> 12) as u8);
                    v.push(0x80 | ((unit >> 6) & 0x3f) as u8);
                    v.push(0x80 | (unit & 0x3f) as u8);
                }
            }
        }
    }
    let length = (v.len() - start - 2) as u16;
    v[start..start + 2].copy_from_slice(&length.to_be_bytes());
}
/// Serialize one `Heightmap.Types` array the way `ChunkSerializer` writes it:
/// 256 nine-bit entries, seven per long, zero-padded to 37 longs, stored
/// relative to the dimension's `min_y`.
fn heightmap_pack_longs(v: &mut Vec<u8>, name: &str, tops: &[i32; 256], min_y: i32) {
    v.push(12); // TAG_Long_Array
    nbt_str(v, name);
    v.extend_from_slice(&37i32.to_be_bytes());
    let per = 64 / 9;
    let mut longs = vec![0u64; 37];
    for (i, t) in tops.iter().enumerate().take(256) {
        let li = i / per;
        let off = (i % per) * 9;
        // tops already = top y+1; the heightmap is stored relative to this dimension's min_y.
        // A Nether/End column top of 70 is 70, not 134 (70 - (-64)).
        let val = (*t - min_y).max(0) as u64;
        if li < 37 {
            longs[li] |= (val & 0x1ff) << off;
        }
    }
    for l in longs {
        v.extend_from_slice(&l.to_be_bytes());
    }
}
/// The chunk packet's heightmaps compound: 1.21.1 carries both `MOTION_BLOCKING`
/// (motion-blocking or fluid) and `WORLD_SURFACE` (any non-air state).
fn heightmaps_nbt_dim(tops: &[i32; 256], ws_tops: &[i32; 256], min_y: i32) -> Vec<u8> {
    let mut v = Vec::new();
    v.push(10); // TAG_Compound — anonymousNbt: contents directly, no root name
    heightmap_pack_longs(&mut v, "MOTION_BLOCKING", tops, min_y);
    heightmap_pack_longs(&mut v, "WORLD_SURFACE", ws_tops, min_y);
    v.push(0);
    v
}

/// Bounded cache of `map_chunk` payloads, keyed by `(dim, cx, cz)` (M3 P5).
///
/// Streaming the same chunk to a second player rebuilt the entire 0x27 payload —
/// heightmaps, the terrain encode, the sign block entities, the light arrays — per
/// player. Because block/sky light also read the 3x3 neighborhood (`world()`,
/// `sky_floors`, and neighbor `GenSources`), a cached entry is keyed by `(dim, cx, cz)`
/// and validated against:
/// 1. the 3x3 `chunk_version_dim` tuple (catches block/torch/lava edits in the chunk or
///    any of its 8 neighbors),
/// 2. `map_chunk_sig(dim, cx, cz, gen)` (catches sign text edits, Overworld spawner
///    changes, and differing neighbor generated-emitter sets in `gen`).
///
/// The entry also retains the chunk's own generated emitters (`own_emitters`) so a cache
/// hit populates the caller's `GenSources` identically to `build_map_chunk` — required
/// by `light_dirty_for_dim` and later `update_light_packet_dim` calls. Bounded to the
/// last 512 chunks, like the band and section stores.
struct MapChunkEntry {
    versions: [u64; 9],
    sig: u64,
    own_emitters: Vec<((i32, i32, i32), u8)>,
    payload: Vec<u8>,
}

#[derive(Default)]
struct MapChunkCache {
    map: HashMap<(DimId, i32, i32, bool), MapChunkEntry>,
    order: std::collections::VecDeque<(DimId, i32, i32, bool)>,
    generation: u64,
}

impl MapChunkCache {
    /// Drop everything if the generator has been replaced since the last entry was stored.
    fn sync_generation(&mut self) {
        let seq = current_gen_seq();
        if self.generation != seq {
            self.map.clear();
            self.order.clear();
            self.generation = seq;
        }
    }
}

static MAP_CHUNK_CACHE: OnceLock<Mutex<MapChunkCache>> = OnceLock::new();

/// Entries retained in the `map_chunk` cache; each payload is a full chunk packet.
const MAP_CHUNK_CACHE_CAP: usize = 512;

fn map_chunk_cache() -> &'static Mutex<MapChunkCache> {
    MAP_CHUNK_CACHE.get_or_init(|| Mutex::new(MapChunkCache::default()))
}

/// Snapshot the 3x3 write versions around `(dim, cx, cz)` under a single lock.
fn chunk_versions_3x3(dim: DimId, cx: i32, cz: i32) -> [u64; 9] {
    let guard = chunk_versions().lock().unwrap();
    let mut out = [0u64; 9];
    let mut i = 0;
    for dx in -1..=1 {
        for dz in -1..=1 {
            out[i] = guard.get(&(dim, cx + dx, cz + dz)).copied().unwrap_or(0);
            i += 1;
        }
    }
    out
}

/// Order-independent FNV-1a signature of the chunk's non-versioned payload inputs:
/// signs in the chunk, Overworld generated spawners (pos + NBT), and the 8 neighbor
/// chunks' generated light emitters in `gen`.
fn map_chunk_sig(dim: DimId, cx: i32, cz: i32, gen: &GenSources) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut sign_acc = 0u64;
    let mut sign_count = 0u64;
    for (k, lines) in signs_in_chunk(dim, cx, cz) {
        let mut h = OFFSET;
        for v in [k.1, k.2, k.3] {
            h = (h ^ v as u64).wrapping_mul(PRIME);
        }
        h = (h ^ k.4 as u64).wrapping_mul(PRIME);
        for line in lines.iter() {
            h = (h ^ 0xff).wrapping_mul(PRIME);
            for b in line.bytes() {
                h = (h ^ b as u64).wrapping_mul(PRIME);
            }
        }
        sign_acc = sign_acc.wrapping_add(h);
        sign_count += 1;
    }
    let mut spawner_acc = 0u64;
    let mut spawner_count = 0u64;
    if dim == DimId::Overworld {
        for ((px, py, pz), ty, nbt) in generated_spawner_chunk_entities(cx, cz) {
            let mut h = OFFSET;
            for v in [px, py, pz] {
                h = (h ^ v as u64).wrapping_mul(PRIME);
            }
            h = (h ^ ty as u64).wrapping_mul(PRIME);
            for &b in &nbt {
                h = (h ^ b as u64).wrapping_mul(PRIME);
            }
            spawner_acc = spawner_acc.wrapping_add(h);
            spawner_count += 1;
        }
    }
    let mut h = OFFSET;
    let light_revision = settled_live_light(dim, cx, cz).map_or(0, |light| light.revision);
    for v in [sign_acc, sign_count, spawner_acc, spawner_count, light_revision] {
        h = (h ^ v).wrapping_mul(PRIME);
    }
    for dx in -1..=1 {
        for dz in -1..=1 {
            if dx == 0 && dz == 0 {
                continue;
            }
            h = (h ^ ((dx + 2) as u64 * 7 + (dz + 2) as u64)).wrapping_mul(PRIME);
            if let Some(list) = gen.get(&(cx + dx, cz + dz)) {
                h = (h ^ list.len() as u64).wrapping_mul(PRIME);
                for ((ex, ey, ez), lum) in list {
                    for v in [*ex as u64, *ey as u64, *ez as u64, *lum as u64] {
                        h = (h ^ v).wrapping_mul(PRIME);
                    }
                }
            }
        }
    }
    h
}

/// Drop every chunk-scoped cache so tests start cold.
#[cfg(test)]
fn clear_chunk_caches_for_tests() {
    map_chunk_cache().lock().unwrap().map.clear();
    map_chunk_cache().lock().unwrap().order.clear();
    op_bands().lock().unwrap().map.clear();
    op_bands().lock().unwrap().order.clear();
    section_store().lock().unwrap().map.clear();
    section_store().lock().unwrap().order.clear();
    sky_floors_cache().lock().unwrap().map.clear();
    sky_floors_cache().lock().unwrap().order.clear();
    *live_light_cache().lock().unwrap() = LiveLightCache::default();
    live_light_engines().lock().unwrap().clear();
    pending_light_edits().lock().unwrap().clear();
}

#[cfg(test)]
fn map_chunk(cx: i32, cz: i32, dim: DimId, gen: &mut GenSources) -> Vec<u8> {
    map_chunk_lane(cx, cz, dim, gen, false)
}

fn map_chunk_lane(
    cx: i32,
    cz: i32,
    dim: DimId,
    gen: &mut GenSources,
    occluded: bool,
) -> Vec<u8> {
    // M3 P5: the 0x27 payload is shared by every player that loads the same chunk at
    // matching 3x3 versions and entity/neighbor-light signatures; a hit also restores
    // `(cx, cz)`'s generated emitters into `gen` so downstream light updates stay exact.
    // `occluded` splits the lanes: a surface player's masked payload must never be
    // served to an underground player nor the other way round.
    let key = (dim, cx, cz, occluded);
    let versions = chunk_versions_3x3(dim, cx, cz);
    let candidate_sig = {
        let mut cache = map_chunk_cache().lock().unwrap();
        cache.sync_generation();
        cache
            .map
            .get(&key)
            .and_then(|e| (e.versions == versions).then_some(e.sig))
    };
    if let Some(want_sig) = candidate_sig {
        let sig = map_chunk_sig(dim, cx, cz, gen);
        if sig == want_sig {
            let mut cache = map_chunk_cache().lock().unwrap();
            cache.sync_generation();
            if let Some(e) = cache.map.get(&key) {
                if e.versions == versions && e.sig == sig {
                    gen.insert((cx, cz), e.own_emitters.clone());
                    return e.payload.clone();
                }
            }
        }
    }
    let payload = build_map_chunk_lane(cx, cz, dim, gen, occluded);
    if chunk_versions_3x3(dim, cx, cz) == versions {
        let sig = map_chunk_sig(dim, cx, cz, gen);
        let own_emitters = gen.get(&(cx, cz)).cloned().unwrap_or_default();
        let mut cache = map_chunk_cache().lock().unwrap();
        cache.sync_generation();
        if cache
            .map
            .insert(
                key,
                MapChunkEntry {
                    versions,
                    sig,
                    own_emitters,
                    payload: payload.clone(),
                },
            )
            .is_none()
        {
            cache.order.push_back(key);
        }
        while cache.order.len() > MAP_CHUNK_CACHE_CAP {
            if let Some(old) = cache.order.pop_front() {
                cache.map.remove(&old);
            }
        }
    }
    payload
}

/// The uncached `map_chunk` build: heightmaps, the terrain encode, the sign block
/// entities and the light arrays, per player.
fn build_map_chunk_lane(
    cx: i32,
    cz: i32,
    dim: DimId,
    gen: &mut GenSources,
    occluded: bool,
) -> Vec<u8> {
    let __t = chunkprof::enabled().then(std::time::Instant::now);
    // The occluded lane's ceiling: decided by the same `hmin` `chunk_data_lane`
    // computes, so block entities and light are stripped over exactly the
    // sections the terrain encode masks.
    let occlude_ceil_y = occluded.then(|| underground_occlude_ceil(dim, cx, cz)).flatten();
    let mut p = Vec::new();
    p.extend_from_slice(&cx.to_be_bytes());
    p.extend_from_slice(&cz.to_be_bytes());
    let mut tops = [0i32; 256];
    let mut ws_tops = [0i32; 256];
    if let Some(c) = worldgen::chunk_cols_dim(dim, cx, cz) {
        // WORLD_SURFACE bounds, per column: `wtop` only tracks solid-or-fluid
        // terrain, so the scan start must also cover the feature layer's
        // highest non-air write (`feature_height(x,z,0)` is that write's y+1)
        // and any live edit that raised the column — both are scan bounds,
        // not answers, because an overlay air cell may sit above them.
        let feature = worldgen::with_gen_dim(dim, |g| {
            // `feat_hm` fills when pending writers flush, so the bound is only
            // complete after this chunk is decorated: `ensure_populated` runs
            // the 3x3 writer box that can touch these cells, otherwise the
            // first packet build scans from `wtop` alone and misses canopies.
            g.ensure_populated(cx, cz, crate::feature::populate);
            let mut v = [i32::MIN; 256];
            for lz in 0..16usize {
                for lx in 0..16usize {
                    let (x, z) = (cx * 16 + lx as i32, cz * 16 + lz as i32);
                    v[lz * 16 + lx] = g.feature_height(x, z, 0);
                }
            }
            v
        })
        .unwrap_or([i32::MIN; 256]);
        let mut edit_tops = [i32::MIN; 256];
        if let Some(reg) = breg() {
            let w = timed_world();
            if let Some(edits) = w.get(&(dim, cx, cz)) {
                for (&cell, &st) in edits {
                    if reg.heightmap_counts(st, HeightmapKind::WorldSurface) {
                        let (ex, ez) = ((cell & 15) as usize, ((cell >> 4) & 15) as usize);
                        let ey = MIN_Y + (cell >> 8) as i32;
                        let i = ez * 16 + ex;
                        edit_tops[i] = edit_tops[i].max(ey);
                    }
                }
            }
        }
        for lz in 0..16usize {
            for lx in 0..16usize {
                let i = lz * 16 + lx;
                // MOTION_BLOCKING is the top of the fluid-inclusive surface, not the density top.
                tops[i] = c.wtop(lx, lz) + 1;
                let (bx, bz) = (cx * 16 + lx as i32, cz * 16 + lz as i32);
                let mut y = c
                    .wtop(lx, lz)
                    .max(feature[i].saturating_sub(1))
                    .max(edit_tops[i])
                    .min(dim.min_y() + dim.height() - 1);
                while y >= dim.min_y()
                    && !breg().is_some_and(|reg| {
                        reg.heightmap_counts(
                            block_at_dim(dim, bx, y, bz),
                            HeightmapKind::WorldSurface,
                        )
                    })
                {
                    y -= 1;
                }
                ws_tops[i] = y + 1;
            }
        }
    }
    p.extend_from_slice(&heightmaps_nbt_dim(&tops, &ws_tops, dim.min_y()));
    let __td = chunkprof::enabled().then(std::time::Instant::now);
    let data = chunk_data_lane(cx, cz, dim, gen, chunk_prefetch_enabled(), occluded);
    if let Some(t) = __td {
        chunkprof::add(&chunkprof::DATA, t);
    }
    write_varint(&mut p, data.len() as i64);
    p.extend_from_slice(&data);
    materialize_pending_generation();
    // block entities ride inside the chunk packet in vanilla — signs
    // would otherwise render blank until edited after each chunk load
    let mut be: Vec<ChunkBlockEntity> = signs_in_chunk(dim, cx, cz)
        .into_iter()
        .map(|((_, x, y, z, front), lines)| {
            let empty: [String; 4] = Default::default();
            let (fl, bl) = if front != 0 {
                (&lines, &empty)
            } else {
                (&empty, &lines)
            };
            let mut n = Vec::new();
            n.push(10);
            sign_side_nbt(&mut n, "front_text", fl);
            sign_side_nbt(&mut n, "back_text", bl);
            nbt_named_byte(&mut n, "is_waxed", 0);
            n.push(0);
            ((x, y, z), 7i64, n)
        })
        .collect();
    // The spawner table holds Overworld positions only. Filing a Nether/End chunk with an
    // Overworld spawner at the same coordinates would send a block entity the client cannot
    // place there.
    if dim == DimId::Overworld {
        let hide_buried = anti_xray_enabled();
        be.extend(
            generated_spawner_chunk_entities(cx, cz)
                .into_iter()
                .filter(|&((x, y, z), _, _)| !hide_buried || block_exposed_dim(dim, x, y, z)),
        );
    }
    // Occluded lane: stronghold chests, spawners and buried bases must not leak
    // through block-entity NBT either — everything under the ceiling is dropped.
    if let Some(ceil) = occlude_ceil_y {
        be.retain(|&((_, y, _), _, _)| y >= ceil);
    }
    write_varint(&mut p, be.len() as i64);
    for ((x, y, z), ty, n) in be {
        p.push(((x & 15) as u8) << 4 | (z & 15) as u8);
        p.extend_from_slice(&(y as i16).to_be_bytes());
        write_varint(&mut p, ty);
        p.extend_from_slice(&n);
    }
    overlay_band(dim, cx, cz);
    let __tl = chunkprof::enabled().then(std::time::Instant::now);
    write_light_arrays_lane(&mut p, cx, cz, dim, gen, occlude_ceil_y);
    if let Some(t) = __tl {
        chunkprof::add(&chunkprof::LIGHT, t);
    }
    if let Some(t) = __t {
        let n = chunkprof::N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        chunkprof::add(&chunkprof::MAP, t);
        if n % 30 == 0 {
            chunkprof::report();
        }
    }
    p
}

// ---------- anti-fullbright ----------
//
// What `antifullbright` can and cannot do. A server cannot stop a client from raising its
// gamma or running a fullbright mod: brightness is drawn on the client from whatever light
// data it holds. What the server controls is which light data it sends. Block light inside
// a sealed generated pocket (a buried lava pocket, a geode's amethyst glow, glow lichen in a
// closed crack) tells a client where that pocket is although no face of it can be seen;
// darkness would hide it from an honest player. So that light is sent as zero until the
// pocket is opened. It does not make caves dark for someone with fullbright - an open cave
// is visible and keeps its real light.
//
// "Sealed": the connected light-passing cells (opacity < 15) around the lit cell number at
// most `SEALED_CAVITY_MAX`, stay inside the chunk's 3x3 light window, and none of them is a
// player edit. Opening a pocket is an edit, so it is no longer sealed, and
// `apply_queued_light_updates` re-sends the real light of the chunks around the opening.

/// Larger closed spaces are treated as open: they are caves, not pockets, and walking them
/// is what the cost bound is for.
const SEALED_CAVITY_MAX: usize = 1024;

struct SealedCavities {
    dim: DimId,
    reg: &'static block_registry::BReg,
    /// The 3x3 chunk window around the chunk being sent.
    x0: i32,
    x1: i32,
    z0: i32,
    z1: i32,
    sealed: HashMap<(i32, i32, i32), bool>,
}

impl SealedCavities {
    fn new(dim: DimId, cx: i32, cz: i32, reg: &'static block_registry::BReg) -> Self {
        SealedCavities {
            dim,
            reg,
            x0: cx * 16 - 16,
            x1: cx * 16 + 31,
            z0: cz * 16 - 16,
            z1: cz * 16 + 31,
            sealed: HashMap::new(),
        }
    }

    fn passes(&self, st: u32) -> bool {
        self.reg.opacity_of(st) < 15
    }

    fn inside(&self, (x, y, z): (i32, i32, i32)) -> bool {
        (self.x0..=self.x1).contains(&x)
            && (self.z0..=self.z1).contains(&z)
            && (self.dim.min_y()..self.dim.min_y() + self.dim.height()).contains(&y)
    }

    /// Whether the light-passing region containing `start` is a sealed generated pocket.
    fn region_sealed(&mut self, start: (i32, i32, i32)) -> bool {
        if let Some(&known) = self.sealed.get(&start) {
            return known;
        }
        let mut seen: HashSet<(i32, i32, i32)> = HashSet::from([start]);
        let mut queue = vec![start];
        let mut sealed = true;
        'walk: while let Some(p) = queue.pop() {
            if overlay_block_dim(self.dim, p.0, p.1, p.2).is_some() {
                sealed = false;
                break;
            }
            for (dx, dy, dz) in [
                (1, 0, 0),
                (-1, 0, 0),
                (0, 1, 0),
                (0, -1, 0),
                (0, 0, 1),
                (0, 0, -1),
            ] {
                let n = (p.0 + dx, p.1 + dy, p.2 + dz);
                if seen.contains(&n) {
                    continue;
                }
                if self.sealed.get(&n) == Some(&false) {
                    sealed = false;
                    break 'walk;
                }
                if !self.inside(n) {
                    sealed = false;
                    break 'walk;
                }
                if self.passes(block_at_dim(self.dim, n.0, n.1, n.2)) {
                    if seen.len() >= SEALED_CAVITY_MAX {
                        sealed = false;
                        break 'walk;
                    }
                    seen.insert(n);
                    queue.push(n);
                }
            }
        }
        for p in seen {
            self.sealed.insert(p, sealed);
        }
        sealed
    }

    /// A lit cell whose light no client could see without opening something.
    fn hidden(&mut self, p: (i32, i32, i32)) -> bool {
        if !self.inside(p) {
            return false;
        }
        if let Some(&known) = self.sealed.get(&p) {
            return known;
        }
        if self.passes(block_at_dim(self.dim, p.0, p.1, p.2)) {
            return self.region_sealed(p);
        }
        if overlay_block_dim(self.dim, p.0, p.1, p.2).is_some() {
            return false;
        }
        [
            (1, 0, 0),
            (-1, 0, 0),
            (0, 1, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
        ]
        .into_iter()
        .all(|(dx, dy, dz)| {
            let n = (p.0 + dx, p.1 + dy, p.2 + dz);
            !self.inside(n)
                && !self.passes(block_at_dim(self.dim, n.0, n.1, n.2))
                || self.inside(n)
                    && (!self.passes(block_at_dim(self.dim, n.0, n.1, n.2))
                        || self.region_sealed(n))
        })
    }
}

/// Zero the block light of sealed generated pockets in `sections` (light-section index,
/// array) of chunk `(cx, cz)`, when `antifullbright` is on. Returns the cells cleared.
fn hide_sealed_block_light<'a>(
    dim: DimId,
    cx: i32,
    cz: i32,
    sky: Option<&[[u8; 2048]]>,
    sections: impl Iterator<Item = (usize, &'a mut [u8; 2048])>,
) -> usize {
    if !ac_anti_fullbright_enabled() {
        return 0;
    }
    let Some(reg) = breg() else { return 0 };
    let mut cavities: Option<SealedCavities> = None;
    let mut cleared = 0;
    for (i, arr) in sections {
        if arr.iter().all(|b| *b == 0) {
            continue;
        }
        let sy = dim.light_min_y() + 16 * i as i32;
        for k in 0..4096usize {
            let shift = (k & 1) * 4;
            if (arr[k >> 1] >> shift) & 15 == 0 {
                continue;
            }
            if sky
                .and_then(|sky| sky.get(i))
                .is_some_and(|s| (s[k >> 1] >> shift) & 15 != 0)
            {
                continue;
            }
            let p = (
                cx * 16 + (k & 15) as i32,
                sy + (k >> 8) as i32,
                cz * 16 + ((k >> 4) & 15) as i32,
            );
            let c = cavities.get_or_insert_with(|| SealedCavities::new(dim, cx, cz, reg));
            if c.hidden(p) {
                arr[k >> 1] &= !(15u8 << shift);
                cleared += 1;
            }
        }
    }
    cleared
}

/// Cells where an edit turned an opaque block into a light-passing one: a sealed pocket
/// next to them may have just been opened. Drained by `apply_queued_light_updates`.
fn light_reveals() -> &'static Mutex<HashMap<DimId, Vec<Position>>> {
    static REVEALS: OnceLock<Mutex<HashMap<DimId, Vec<Position>>>> = OnceLock::new();
    REVEALS.get_or_init(Mutex::default)
}

/// The block-light half of a light update for a whole chunk: lit sections in the mask,
/// all-zero ones in the empty mask (vanilla's encoding for them).
fn block_light_update(chunk: Chunk, block: &[[u8; 2048]]) -> Vec<u8> {
    let words = block.len().div_ceil(64);
    let mut mask = vec![0u64; words];
    let mut empty = vec![0u64; words];
    let mut lit = Vec::new();
    for (i, section) in block.iter().enumerate() {
        if section.iter().all(|b| *b == 0) {
            empty[i / 64] |= 1 << (i % 64);
        } else {
            mask[i / 64] |= 1 << (i % 64);
            lit.push(section);
        }
    }
    encode_light_update_multi(
        chunk,
        [Vec::new(), mask, Vec::new(), empty],
        [].into_iter(),
        lit.into_iter(),
    )
}

/// The light half of a chunk packet, shared with `update_light_packet`: four section masks, then
/// sky and block light as two counted lists of 2048-byte arrays.
#[cfg(test)]
fn write_light_arrays(p: &mut Vec<u8>, cx: i32, cz: i32, dim: DimId, gen: &GenSources) {
    write_light_arrays_lane(p, cx, cz, dim, gen, None)
}

/// `write_light_arrays` for one occlusion lane: `occlude_ceil` masks every light
/// section whose whole 16-block band sits under it, so cave lava or torch glow
/// in a masked section cannot leak through the light arrays either. Masked
/// sections are all-zero and travel in the empty masks like any unlit section.
fn write_light_arrays_lane(
    p: &mut Vec<u8>,
    cx: i32,
    cz: i32,
    dim: DimId,
    gen: &GenSources,
    occlude_ceil: Option<i32>,
) {
    // Single-word fast path keeps the <=63-section dims (every vanilla world)
    // allocation-free and byte-identical; `light_mask` saturates at bit 62, so
    // exactly-64 and taller dims must take `light_mask_longs` for full bits.
    let all = if dim.light_sections() < 64 {
        vec![dim.light_mask() as u64]
    } else {
        dim.light_mask_longs()
    };
    let has_sky = worldgen::has_skylight(dim);
    let __tsky = chunkprof::enabled().then(std::time::Instant::now);
    let mut sky = if has_sky {
        sky_light_sections_dim(dim, cx, cz)
    } else {
        Vec::new()
    };
    if let Some(t) = __tsky {
        chunkprof::add(&chunkprof::SKY, t);
    }
    let __tbl = chunkprof::enabled().then(std::time::Instant::now);
    let mut block = block_light_sections_dim(dim, cx, cz, gen);
    if let Some(t) = __tbl {
        chunkprof::add(&chunkprof::BLOCKLIGHT, t);
    }
    if let Some(ceil) = occlude_ceil {
        let lmin = dim.light_min_y();
        let occluded = |i: usize| lmin + 16 * (i as i32 + 1) <= ceil;
        for (i, arr) in sky.iter_mut().enumerate() {
            if occluded(i) {
                arr.fill(0);
            }
        }
        for (i, arr) in block.iter_mut().enumerate() {
            if occluded(i) {
                arr.fill(0);
            }
        }
    }
    hide_sealed_block_light(
        dim,
        cx,
        cz,
        has_sky.then_some(&sky[..]),
        block.iter_mut().enumerate(),
    );
    // A section that is all zero goes in the empty mask with no array: vanilla's own
    // encoding, saving 2 KB per empty section before compression.
    let split = |sections: &[[u8; 2048]]| {
        let mut mask = vec![0u64; all.len()];
        let mut empty = vec![0u64; all.len()];
        for (i, s) in sections.iter().enumerate() {
            if s.iter().all(|b| *b == 0) {
                empty[i / 64] |= 1 << (i % 64);
            } else {
                mask[i / 64] |= 1 << (i % 64);
            }
        }
        for (m, a) in mask.iter_mut().zip(all.iter()) {
            *m &= a;
        }
        for (e, a) in empty.iter_mut().zip(all.iter()) {
            *e &= a;
        }
        (mask, empty)
    };
    let (sky_mask, sky_empty) = split(&sky);
    let (block_mask, block_empty) = split(&block);
    for m in [&sky_mask, &block_mask, &sky_empty, &block_empty] {
        write_light_bitset(p, m);
    }
    for (sections, mask) in [(&sky, &sky_mask), (&block, &block_mask)] {
        write_varint(
            p,
            mask.iter().map(|w| w.count_ones() as i64).sum::<i64>(),
        );
        for (i, sec) in sections.iter().enumerate() {
            if mask[i / 64] & (1 << (i % 64)) != 0 {
                write_varint(p, 2048);
                p.extend_from_slice(sec);
            }
        }
    }
}

#[cfg(test)] // the Overworld-named seam for light tests
fn update_light_packet(cx: i32, cz: i32, gen: &GenSources) -> Vec<u8> {
    update_light_packet_dim(DimId::Overworld, cx, cz, gen)
}

#[cfg(test)]
fn update_light_packet_dim(dim: DimId, cx: i32, cz: i32, gen: &GenSources) -> Vec<u8> {
    update_light_packet_lane(dim, cx, cz, gen, None)
}

/// `update_light_packet_dim` for one occlusion lane: the sections a surface
/// player's chunk packet shows as solid rock must not glow in a later light
/// update, so the same ceiling masks them to zero before the packet is built.
fn update_light_packet_lane(
    dim: DimId,
    cx: i32,
    cz: i32,
    gen: &GenSources,
    occlude_ceil: Option<i32>,
) -> Vec<u8> {
    let mut block = block_light_sections_dim(dim, cx, cz, gen);
    if let Some(ceil) = occlude_ceil {
        let lmin = dim.light_min_y();
        for (i, arr) in block.iter_mut().enumerate() {
            if lmin + 16 * (i as i32 + 1) <= ceil {
                arr.fill(0);
            }
        }
    }
    hide_sealed_block_light(dim, cx, cz, None, block.iter_mut().enumerate());
    block_light_update((cx, cz), &block)
}

/// Protocol `BitSet` (`long[]`) on the light-update wire: a VarInt word count
/// then that many big-endian u64 words, trailing all-zero words trimmed — the
/// same shape `BitSet.toLongArray` produces. A single-word mask is byte-for-byte
/// what the old one-long writers emitted (`1` + the word, or `0` when empty).
fn write_light_bitset(p: &mut Vec<u8>, words: &[u64]) {
    let kept = words.iter().rposition(|w| *w != 0).map_or(0, |i| i + 1);
    write_varint(p, kept as i64);
    for w in &words[..kept] {
        p.extend_from_slice(&w.to_be_bytes());
    }
}

fn encode_light_update<'a>(
    (cx, cz): Chunk,
    masks: [u64; 4],
    sky: impl ExactSizeIterator<Item = &'a [u8; 2048]>,
    block: impl ExactSizeIterator<Item = &'a [u8; 2048]>,
) -> Vec<u8> {
    encode_light_update_multi(
        (cx, cz),
        masks.map(|m| {
            if m == 0 {
                Vec::new()
            } else {
                vec![m]
            }
        }),
        sky,
        block,
    )
}

/// `encode_light_update` over multi-word masks so a dimension with more than
/// 64 light sections (a `dimension_type` taller than 1,008 blocks) encodes its
/// whole `long[]` instead of truncating past the first word.
fn encode_light_update_multi<'a>(
    (cx, cz): Chunk,
    masks: [Vec<u64>; 4],
    sky: impl ExactSizeIterator<Item = &'a [u8; 2048]>,
    block: impl ExactSizeIterator<Item = &'a [u8; 2048]>,
) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, cx as i64);
    write_varint(&mut p, cz as i64);
    for mask in &masks {
        write_light_bitset(&mut p, mask);
    }
    write_varint(&mut p, sky.len() as i64);
    for section in sky {
        write_varint(&mut p, section.len() as i64);
        p.extend_from_slice(section);
    }
    write_varint(&mut p, block.len() as i64);
    for section in block {
        write_varint(&mut p, section.len() as i64);
        p.extend_from_slice(section);
    }
    p
}

// Live edits carry the previous light properties so a bounded light window can
// be initialized from the actual pre-edit world, including generated emitters.
type PendingLightEdits = HashMap<DimId, HashMap<Position, (LightBlock, LightBlock)>>;

fn pending_light_edits() -> &'static Mutex<PendingLightEdits> {
    static EDITS: OnceLock<Mutex<PendingLightEdits>> = OnceLock::new();
    EDITS.get_or_init(Mutex::default)
}

fn light_properties(reg: &BReg, state: u32) -> LightBlock {
    let opacity = reg.opacity_of(state);
    let emission = reg.luminance_of(state);
    let name = reg.name_of(state);
    let half = |value: Option<String>| match value.as_deref() {
        Some("top") => Some(Half::Top),
        Some("bottom") => Some(Half::Bottom),
        _ => None,
    };
    let shape = if name.ends_with("_slab") {
        let kind = reg.get_prop(state, "type");
        if kind.as_deref() == Some("double") {
            FaceOcclusion::FULL
        } else {
            half(kind).map_or(FaceOcclusion::EMPTY, FaceOcclusion::slab)
        }
    } else if name.ends_with("_stairs") {
        let facing = match reg.get_prop(state, "facing").as_deref() {
            Some("north") => Facing::North,
            Some("east") => Facing::East,
            Some("south") => Facing::South,
            Some("west") => Facing::West,
            _ => return LightBlock::new(opacity, emission),
        };
        let shape = match reg.get_prop(state, "shape").as_deref() {
            Some("inner_left") => StairShape::InnerLeft,
            Some("inner_right") => StairShape::InnerRight,
            Some("outer_left") => StairShape::OuterLeft,
            Some("outer_right") => StairShape::OuterRight,
            _ => StairShape::Straight,
        };
        half(reg.get_prop(state, "half")).map_or(FaceOcclusion::EMPTY, |half| {
            FaceOcclusion::stairs(half, facing, shape)
        })
    } else {
        FaceOcclusion::EMPTY
    };
    LightBlock::with_occlusion(opacity, emission, shape)
}

fn note_light_edit(dim: DimId, pos: Position, old: u32, new: u32) {
    if old == new || !(dim.min_y()..dim.min_y() + dim.height()).contains(&pos.y) {
        return;
    }
    let Some(reg) = breg() else { return };
    if reg.opacity_of(old) == 15 && reg.opacity_of(new) < 15 && ac_anti_fullbright_enabled() {
        light_reveals()
            .lock()
            .unwrap()
            .entry(dim)
            .or_default()
            .push(pos);
    }
    let old = light_properties(reg, old);
    let new = light_properties(reg, new);
    if old != new {
        pending_light_edits()
            .lock()
            .unwrap()
            .entry(dim)
            .or_default()
            .entry(pos)
            .and_modify(|edit| edit.1 = new)
            .or_insert((old, new));
    }
}

struct SettledLiveLight {
    versions: [u64; 9],
    revision: u64,
    sky: Vec<[u8; 2048]>,
    block: Vec<[u8; 2048]>,
}

#[derive(Default)]
struct LiveLightCache {
    generation: u64,
    chunks: HashMap<DimChunkKey, Arc<SettledLiveLight>>,
    order: VecDeque<DimChunkKey>,
}

fn live_light_cache() -> &'static Mutex<LiveLightCache> {
    static LIGHT: OnceLock<Mutex<LiveLightCache>> = OnceLock::new();
    LIGHT.get_or_init(Mutex::default)
}

fn settled_live_light(dim: DimId, cx: i32, cz: i32) -> Option<Arc<SettledLiveLight>> {
    let light = {
        let cache = live_light_cache().lock().unwrap();
        if cache.generation != current_gen_seq() {
            return None;
        }
        cache.chunks.get(&(dim, cx, cz))?.clone()
    };
    (light.versions == chunk_versions_3x3(dim, cx, cz)).then_some(light)
}

fn light_halo(chunks: impl IntoIterator<Item = Chunk>) -> impl Iterator<Item = Chunk> {
    chunks.into_iter().flat_map(|(cx, cz)| {
        (-1..=1).flat_map(move |dx| (-1..=1).map(move |dz| (cx + dx, cz + dz)))
    })
}

fn chunk_in_light_reach((cx, cz): Chunk, p: Position) -> bool {
    let x = cx * 16;
    let z = cz * 16;
    p.x.abs_diff(p.x.clamp(x, x + 15))
        .saturating_add(p.z.abs_diff(p.z.clamp(z, z + 15)))
        <= 14
}

fn pre_edit_light_chunk(
    dim: DimId,
    (cx, cz): Chunk,
    reg: &BReg,
    props: &mut HashMap<u32, LightBlock>,
    previous: &HashMap<Position, LightBlock>,
) -> Vec<LightBlock> {
    let overlay = world()
        .lock()
        .unwrap()
        .get(&(dim, cx, cz))
        .cloned()
        .unwrap_or_default();
    let min_y = dim.min_y();
    let mut blocks = Vec::with_capacity(dim.height() as usize * 256);
    let flat_heights = (dim == DimId::Overworld && !worldgen::enabled()).then(|| {
        std::array::from_fn::<_, 256, _>(|i| {
            height_at(cx * 16 + (i & 15) as i32, cz * 16 + (i >> 4) as i32)
        })
    });
    for sy in (min_y..min_y + dim.height()).step_by(16) {
        let mut rows = [0u32; 4096];
        if let Some(section) = stored_section(dim, cx, cz, sy) {
            section.unpack_into(&mut rows);
        } else if let Some(heights) = &flat_heights {
            for (i, state) in rows.iter_mut().enumerate() {
                let x = cx * 16 + (i & 15) as i32;
                let z = cz * 16 + ((i >> 4) & 15) as i32;
                *state = column_state(x, sy + (i >> 8) as i32, z, heights[i & 255]);
            }
        } else if let Some(terrain) =
            worldgen::terrain_section_dim(dim, cx, sy, cz, feature::populate)
        {
            rows = *terrain;
        }
        for (i, state) in rows.into_iter().enumerate() {
            let local = ((sy - MIN_Y) as u32) << 8 | i as u32;
            let state = overlay.get(&local).copied().unwrap_or(state);
            blocks.push(
                *props
                    .entry(state)
                    .or_insert_with(|| light_properties(reg, state)),
            );
        }
    }
    for (pos, old) in previous {
        if pos.chunk() == (cx, cz) {
            let i = ((pos.y - min_y) as usize) << 8
                | ((pos.z & 15) as usize) << 4
                | (pos.x & 15) as usize;
            blocks[i] = *old;
        }
    }
    blocks
}

/// Per-dimension live light engine, persisted across `apply_queued_light_updates`
/// calls. Rebuilding it every call re-evaluated ~98k cells per neighbour chunk
/// (`pre_edit_light_chunk`) plus a full-window `rebuild()` for a handful of
/// single-block edits — ~57 ms/tick under load (TICKPROF `block_updates`). The
/// engine is incremental (`edit_batch`/`relight_patch`; the
/// `assert_matches_full_rebuild` test oracle pins incremental==rebuild), so
/// keeping it across calls reduces the steady-state cost to the edits
/// themselves. It is rebuilt from `neighbors` whenever a fresh build could
/// disagree: the generator changed, a needed neighbour is missing (a missing
/// chunk is a hard light boundary, not a correct-but-stale one), or the engine
/// grew past the cap — extra chunks only add accurate propagation context, but
/// bounding it keeps memory steady.
struct LiveLightEngine {
    generation: u64,
    /// Chunks the engine was built with. `engine.chunks` is not publicly
    /// inspectable, so coverage is tracked here.
    chunks: HashSet<Chunk>,
    engine: IncrementalLighting,
}

const LIVE_LIGHT_ENGINE_CAP: usize = 512;

fn live_light_engines() -> &'static Mutex<HashMap<DimId, LiveLightEngine>> {
    static ENGINES: OnceLock<Mutex<HashMap<DimId, LiveLightEngine>>> = OnceLock::new();
    ENGINES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn apply_queued_light_updates() {
    if pending_light_edits().lock().unwrap().is_empty() {
        return;
    }
    let audience = client_snapshot();
    let mut held: HashMap<DimId, HashSet<Chunk>> = HashMap::new();
    for client in &audience {
        // Do not hold the dimension lock while waiting on a chunk stream.
        let dim = *client.dim.lock().unwrap();
        held.entry(dim)
            .or_default()
            .extend(client.chunks.lock().unwrap().iter().copied());
    }
    let Some(reg) = breg() else { return };
    let generation = current_gen_seq();
    let work = with_save_guard(|| {
        let mut work = Vec::new();
        let edits = std::mem::take(&mut *pending_light_edits().lock().unwrap());
        let mut props = HashMap::new();
        for (dim, edits) in edits {
            let Some(held) = held.get(&dim) else { continue };
            let (old, new): (HashMap<_, _>, Vec<_>) = edits
                .into_iter()
                .filter(|(_, (old, new))| old != new)
                .map(|(pos, (old, new))| ((pos, old), (pos, new)))
                .unzip();
            let targets: Vec<_> = held
                .iter()
                .copied()
                .filter(|chunk| {
                    new.iter()
                        .any(|(pos, _)| chunk_in_light_reach(*chunk, *pos))
                })
                .collect();
            if targets.is_empty() {
                continue;
            }
            // Only visit terrain that streaming or a block write already
            // opened. Lighting must not advance world generation at the border.
            let edited: HashSet<_> = new.iter().map(|(pos, _)| pos.chunk()).collect();
            let neighbors: HashSet<_> = light_halo(targets.iter().copied())
                .filter(|chunk| held.contains(chunk) || edited.contains(chunk))
                .collect();
            let snapshots: Vec<_> = targets
                .iter()
                .copied()
                .filter(|chunk| light_halo([*chunk]).all(|neighbor| neighbors.contains(&neighbor)))
                .map(|(cx, cz)| ((cx, cz), chunk_versions_3x3(dim, cx, cz)))
                .collect();
            // Reuse the persisted engine when it covers every chunk a fresh
            // build would visit; otherwise rebuild it from exactly `neighbors`,
            // as before. A chunk's LightBlock array changes only through
            // `note_light_edit`, and each such change arrives in `edits`, so a
            // covered engine is still current.
            let covered = live_light_engines()
                .lock()
                .unwrap()
                .get(&dim)
                .is_some_and(|entry| {
                    entry.generation == generation
                        && entry.chunks.len() <= LIVE_LIGHT_ENGINE_CAP
                        && neighbors.iter().all(|chunk| entry.chunks.contains(chunk))
                });
            let terrain = (!covered).then(|| {
                neighbors
                    .iter()
                    .copied()
                    .map(|chunk| {
                        (
                            chunk,
                            pre_edit_light_chunk(dim, chunk, reg, &mut props, &old),
                        )
                    })
                    .collect::<Vec<_>>()
            });
            work.push((dim, targets, snapshots, neighbors, terrain, new));
        }
        work
    });

    let mut engines = live_light_engines().lock().unwrap();
    for (dim, targets, snapshots, neighbors, terrain, new) in work {
        let entry = engines.entry(dim).or_insert_with(|| LiveLightEngine {
            generation,
            chunks: HashSet::new(),
            engine: IncrementalLighting::from_chunks(
                dim.min_y(),
                dim.height(),
                worldgen::has_skylight(dim),
                Vec::new(),
            ),
        });
        if let Some(terrain) = terrain {
            entry.engine = IncrementalLighting::from_chunks(
                dim.min_y(),
                dim.height(),
                worldgen::has_skylight(dim),
                terrain,
            );
            entry.chunks = neighbors;
            entry.generation = generation;
        }
        let held_targets = targets.clone();
        let updates = entry.engine.edit_batch(new, targets);
        {
            let mut cache = live_light_cache().lock().unwrap();
            if cache.generation != generation {
                *cache = LiveLightCache {
                    generation,
                    ..Default::default()
                };
            }
            for (chunk @ (cx, cz), versions) in snapshots {
                let key = (dim, cx, cz);
                if !cache.chunks.contains_key(&key)
                    && !updates.chunks.iter().any(|u| u.chunk == chunk)
                {
                    continue;
                }
                let light = SettledLiveLight {
                    versions,
                    revision: versions.into_iter().max().unwrap_or(0),
                    sky: entry.engine.sections(chunk, LightKind::Sky),
                    block: entry.engine.sections(chunk, LightKind::Block),
                };
                if cache.chunks.insert(key, Arc::new(light)).is_none() {
                    cache.order.push_back(key);
                }
            }
            while cache.order.len() > MAP_CHUNK_CACHE_CAP {
                if let Some(chunk) = cache.order.pop_front() {
                    cache.chunks.remove(&chunk);
                }
            }
        }
        for mut update in updates.chunks {
            let (cx, cz) = update.chunk;
            hide_sealed_block_light(
                dim,
                cx,
                cz,
                None,
                update.block_sections.iter_mut().map(|(i, a)| (*i, a)),
            );
            let packet = encode_light_update(
                update.chunk,
                [
                    update.sky_mask,
                    update.block_mask,
                    update.empty_sky_mask,
                    update.empty_block_mask,
                ],
                update.sky_sections.iter().map(|(_, array)| array),
                update.block_sections.iter().map(|(_, array)| array),
            );
            broadcast_chunk_dim_in(dim, &audience, cx, cz, |_| (0x2a, packet.clone()));
        }
        // antifullbright: an edit that opened a sealed pocket does not change the light in
        // it, so the update above may not cover the sections the client holds as zero.
        // Re-send the chunks around each opening in full; they are no longer sealed.
        let reveals = light_reveals().lock().unwrap().remove(&dim).unwrap_or_default();
        if !reveals.is_empty() {
            for &chunk @ (cx, cz) in &held_targets {
                if !reveals.iter().any(|p| chunk_in_light_reach(chunk, *p)) {
                    continue;
                }
                let mut block = entry.engine.sections(chunk, LightKind::Block);
                let sky = entry.engine.sections(chunk, LightKind::Sky);
                hide_sealed_block_light(dim, cx, cz, Some(&sky), block.iter_mut().enumerate());
                let packet = block_light_update(chunk, &block);
                broadcast_chunk_dim_in(dim, &audience, cx, cz, |_| (0x2a, packet.clone()));
            }
        }
    }
}

/// Chunks already generated whose light the chunk at `(cx, cz)` can change. A neighbour is
/// affected exactly when one of this chunk's emitters lies inside the neighbour's light search
/// area; the light the client already holds for it predates these sources, so it is recomputed
/// and re-sent. Neighbours generated later name this chunk in their own call, so every ordered
/// pair is covered once.
#[cfg(test)]
fn light_dirty_for(gen: &GenSources, cx: i32, cz: i32) -> Vec<(i32, i32)> {
    light_dirty_for_dim(DimId::Overworld, gen, cx, cz)
}

/// [`light_dirty_for`] for one dimension. A Nether emitter at y=32 is inside the Nether's
/// 0..256 window but outside the Overworld's search window; using the Overworld predicate
/// would skip neighbours a Nether torch actually lights.
fn light_dirty_for_dim(dim: DimId, gen: &GenSources, cx: i32, cz: i32) -> Vec<(i32, i32)> {
    let Some(sources) = gen.get(&(cx, cz)) else {
        return Vec::new();
    };
    if sources.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for dx in -1..=1 {
        for dz in -1..=1 {
            if dx == 0 && dz == 0 {
                continue;
            }
            let (nx, nz) = (cx + dx, cz + dz);
            if !gen.contains_key(&(nx, nz)) {
                continue;
            }
            if sources
                .iter()
                .any(|(p, _)| within_search_dim(dim, nx, nz, p.0, p.1, p.2))
            {
                out.push((nx, nz));
            }
        }
    }
    out
}

// the window parameters are read together every batch; a struct would add a
// name without removing a parameter
#[expect(clippy::too_many_arguments)]
/// Worker count for terrain prefetch: `MC_SPINE_GEN_JOBS` (0/1 = inline), else
/// `min(available_parallelism, 8)`. Terrain is pure, so this only trades threads for wall
/// time and never changes what is generated; the count itself is owned by
/// `worldgen::terrain_jobs` so the dump path and the delivery path cannot disagree.
///
/// The parallel arm is the **default**. It used to be opt-in on the grounds that
/// `WARM=1 load_compare.sh` measured CPU 21.9 s -> 23.3 s with 4 workers, and CPU seconds
/// are what that lane measures; that stands as a measurement of *CPU*, and the trade it
/// describes is the one this change takes deliberately: the same work, spread over
/// threads, with wall clock down several times and CPU slightly up. `MC_SPINE_GEN_JOBS=1`
/// restores the serial arm for a CPU-sensitive comparison.
fn gen_workers() -> usize {
    worldgen::terrain_jobs()
}

/// How many chunks one prefetch round covers: two per worker, so a round has a second
/// chunk ready when a worker finishes its first. At most `2 * 8 = 16` chunks, i.e. 6 MiB
/// of base rows (`worldgen::TERRAIN_STORE_BOUND` holds 8 MiB).
fn terrain_group(workers: usize) -> usize {
    (workers * 2).max(1)
}

/// Worker count for the **delivery** path's terrain prefetch, which runs on the tick thread.
///
/// Opt-in (`MC_SPINE_GEN_JOBS`), unlike the offline dump path. Reason, measured on this
/// host: the delivery path's prefetch runs between the client's batch and the send loop, so
/// a round of chunks is built while the tick thread waits. That is a *latency* change to the
/// tick, and it moved `tick_tests::wall_clock_boundaries_are_gated_on_elapsed_time_not_tick_count`
/// — whose assertion is "40 ticks cost microseconds against a 1 s period" — over its
/// boundary once the suite ran the whole delivery path on 8 workers. The offline dump is
/// what this slice measures and it is default; the delivery arm stays a flag until a
/// tick-latency measurement (not a throughput one) says it is safe.
fn delivery_prefetch_jobs() -> usize {
    std::env::var("MC_SPINE_GEN_JOBS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(1)
}

/// Build the group's **base terrain** off the lock, in parallel, and hand it to the
/// generator: the columns each chunk's feature pass will read, plus the section rows the
/// send loop is about to ask for.
///
/// Features stay inside the sequential loop: their order is pinned, they read other
/// chunks' writes, and the population discipline is a serial demand scan. Only the
/// terrain half — density, material rules, aquifers, surface rules — is parallel, and it is
/// a pure function of the immutable data and the seed (vanilla draws the same line:
/// `NoiseBasedChunkGenerator.fillFromNoise` runs on the background executor while
/// `FEATURES`/`CARVERS` complete synchronously on the thread that popped the task).
fn prefetch_terrain_group(
    news: &[(i32, i32)],
    lo: i32,
    hi: i32,
    workers: usize,
    round: Option<u64>,
) {
    prefetch_terrain_group_dim(DimId::Overworld, news, lo, hi, workers, round)
}

/// [`prefetch_terrain_group`] for one dimension: the snapshot, the scratch generators and
/// the beard resolver all have to come from that dimension's registry slot, or a Nether
/// window would be built from Overworld noise and installed into the Overworld generator.
fn prefetch_terrain_group_dim(
    dim: DimId,
    news: &[(i32, i32)],
    lo: i32,
    hi: i32,
    workers: usize,
    round: Option<u64>,
) {
    if news.is_empty() {
        return;
    }
    let Some(data) = worldgen::data_snapshot_dim(dim) else {
        return;
    };
    // `jobs = 1` builds the same rows on one scratch generator instead of eight. It is not
    // skipped: the base rows carry the terrain stage's `markPosForPostprocessing` decisions,
    // so a serial run that let the sections be built lazily would file those marks after the
    // chunk was already promoted, and `MC_SPINE_WG_GEN_JOBS=1` vs `8` would stop being a
    // comparison of the same thing.
    let jobs = workers.max(1);
    let resolver = worldgen::with_gen_dim(dim, |g| g.beard_resolver()).flatten();
    let cols = worldgen::prefetch_cols_with(&data, resolver, news, jobs, round);
    let chunks =
        worldgen::prefetch_terrain(&data, resolver, news, &cols, lo, hi, jobs, round);
    let _ = worldgen::with_gen_dim(dim, |g| g.install_prefetched(chunks));
}







// ---------- RCON (parity-fixture control plane) ----------
static RCON_AUTH: connection_limits::AuthThrottle = connection_limits::AuthThrottle::new();

fn peer_label(s: &TcpStream) -> String {
    s.peer_addr()
        .map_or_else(|_| "unknown".to_string(), |a| a.ip().to_string())
}
fn rcon_conn(mut s: TcpStream, pass: String) {
    use std::io::Write;
    let peer_ip = s.peer_addr().ok().map(|a| a.ip());
    let peer = peer_ip.map_or_else(|| "unknown".to_string(), |ip| ip.to_string());
    s.set_nodelay(true).ok();
    s.set_read_timeout(Some(Duration::from_secs(10))).ok();
    s.set_write_timeout(Some(Duration::from_secs(10))).ok();
    // Never treat an empty secret as successful RCON authentication.
    let mut authed = false;
    loop {
        let mut lb = [0u8; 4];
        if s.read_exact(&mut lb).is_err() {
            return;
        }
        let len = i32::from_le_bytes(lb);
        if !(4..=8200).contains(&len) {
            return;
        }
        let mut body = vec![0u8; len as usize];
        if s.read_exact(&mut body).is_err() {
            return;
        }
        if body.len() < 10 {
            return;
        }
        let id = i32::from_le_bytes(body[..4].try_into().unwrap());
        let ty = i32::from_le_bytes(body[4..8].try_into().unwrap());
        let cmd = String::from_utf8_lossy(&body[8..body.len().saturating_sub(2)]).to_string();
        let (rid, rty, out) = if !authed {
            if ty == 3 && connection_limits::ct_eq(cmd.as_bytes(), pass.as_bytes()) {
                authed = true;
                if let Some(ip) = peer_ip {
                    RCON_AUTH.record_success(ip);
                }
                eprintln!("[rcon] {peer} authenticated");
                (id, 2i32, String::new())
            } else if ty == 3 {
                // never log the attempted password
                let fails = peer_ip.map_or(0, |ip| {
                    RCON_AUTH.record_failure(
                        ip,
                        Instant::now(),
                        connection_limits::rcon_max_failures(),
                        connection_limits::rcon_lockout(),
                    )
                });
                eprintln!("[security] rcon authentication failed from {peer} (failure #{fails})");
                // slow a guessing client down; the lockout stops a patient one
                std::thread::sleep(Duration::from_secs(1));
                (id, 2i32, String::new())
            }
            // wrong pass handled below
            else {
                return;
            }
        } else {
            match ty {
                2 => (id, 0i32, {
                    // audit trail: who ran what (Debug-escaped so a command
                    // cannot forge log lines)
                    eprintln!("[rcon] {peer} exec: {cmd:?}");
                    run_command_ctx(
                        &CmdCtx {
                            me: None,
                            x: 0.5,
                            y: 16.0,
                            z: 0.5,
                                        },
                        &format!("/{}", cmd.trim_start_matches('/')),
                        &mut None,
                    )
                }),
                _ => (id, 0i32, String::new()),
            }
        };
        let rid = if ty == 3 && !authed { -1 } else { rid };
        let mut p = Vec::with_capacity(out.len() + 14);
        p.extend_from_slice(&(10 + out.len() as i32).to_le_bytes());
        p.extend_from_slice(&rid.to_le_bytes());
        p.extend_from_slice(&rty.to_le_bytes());
        p.extend_from_slice(out.as_bytes());
        p.extend_from_slice(&[0, 0]);
        if s.write_all(&p).is_err() {
            return;
        }
        if ty == 3 && rid == -1 {
            return;
        }
    }
}
fn rcon_listener() {
    let Some(port) = std::env::var("MC_SPINE_RCON_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
    else {
        return;
    };
    let pass = std::env::var("MC_SPINE_RCON_PASS").unwrap_or_default();
    if pass.trim().is_empty() {
        eprintln!("[rcon] refusing to start without a nonempty MC_SPINE_RCON_PASS");
        return;
    }
    let bind = std::env::var("MC_SPINE_RCON_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1".into());
    if let Err(why) =
        connection_limits::check_bind("RCON", &bind, connection_limits::public_bind_allowed())
    {
        eprintln!("[rcon] {why}");
        return;
    }
    let Ok(l) = TcpListener::bind((bind.as_str(), port)) else {
        eprintln!("[rcon] bind failed on {bind}:{port}");
        return;
    };
    eprintln!("[rcon] listening {bind}:{port}");
    static RCON_ACTIVE: AtomicUsize = AtomicUsize::new(0);
    for s in l.incoming().flatten() {
        // a locked-out address does not even get a handler thread
        if let Ok(a) = s.peer_addr() {
            if let Some(left) = RCON_AUTH.locked_for(a.ip(), Instant::now()) {
                log_refusal(
                    &a.ip().to_string(),
                    &format!("rcon lockout, {}s left", left.as_secs()),
                );
                drop(s);
                continue;
            }
        }
        if RCON_ACTIVE.fetch_add(1, Ordering::SeqCst) >= 8 {
            RCON_ACTIVE.fetch_sub(1, Ordering::SeqCst);
            drop(s);
            continue;
        }
        let pass = pass.clone();
        std::thread::spawn(move || {
            let _slot = ConnectionSlot(&RCON_ACTIVE);
            rcon_conn(s, pass);
        });
    }
}
// The caller may observe a stage, but must execute it exactly once and in order.
/// Delivery-path and tick-body profilers, both off unless their switch is set.
///
/// Measured 2026-10-28 with one moving viewer (`tools/load_compare.sh` conditions):
/// ~29.5 s of CPU in a 20 s window, of which terrain+features are ~1.7 s. These two
/// counters say where the rest is: `MC_SPINE_CHUNK_PROFILE=1` splits the chunk packet,
/// `MC_SPINE_TICK_PROFILE=1` splits the tick body.
pub(crate) mod chunkprof {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub(super) static MAP: AtomicU64 = AtomicU64::new(0);
    pub(super) static DATA: AtomicU64 = AtomicU64::new(0);
    pub(super) static SECTION: AtomicU64 = AtomicU64::new(0);
    pub(super) static SECTION_N: AtomicU64 = AtomicU64::new(0);
    /// Every `terrain_section` call from the delivery path: base, the +x prefetch, and the
    /// section above. A count above one per section means the same section is computed twice.
    pub(super) static TERRAIN_N: AtomicU64 = AtomicU64::new(0);
    pub(super) static ENCODE: AtomicU64 = AtomicU64::new(0);
    pub(super) static READ_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static READ_BAND_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static READ_BAND_N: AtomicU64 = AtomicU64::new(0);
    pub(super) static BAND_LOOKUP_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static OVERLAY_READ_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static BASE_STATE_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static BASE_STATE_N: AtomicU64 = AtomicU64::new(0);
    pub(super) static ENC_SECTION: AtomicU64 = AtomicU64::new(0);
    pub(super) static BIOMES: AtomicU64 = AtomicU64::new(0);
    pub(super) static BAND_INSTALL: AtomicU64 = AtomicU64::new(0);
    pub(super) static FASTPATH: AtomicU64 = AtomicU64::new(0);
    pub(super) static FASTPATH_N: AtomicU64 = AtomicU64::new(0);
    pub(super) static OVERLAY_APPLY: AtomicU64 = AtomicU64::new(0);
    pub(super) static HIDDEN_N: AtomicU64 = AtomicU64::new(0);
    pub(super) static EXPOSURE_READS: AtomicU64 = AtomicU64::new(0);
    pub(super) static EXPOSURE_READ_BASE: AtomicU64 = AtomicU64::new(0);
    pub(super) static LIGHT: AtomicU64 = AtomicU64::new(0);
    pub(super) static RE_LIGHT: AtomicU64 = AtomicU64::new(0);
    pub(super) static RE_LIGHT_N: AtomicU64 = AtomicU64::new(0);
    pub(super) static SKY: AtomicU64 = AtomicU64::new(0);
    pub(super) static BLOCKLIGHT: AtomicU64 = AtomicU64::new(0);
    pub(super) static FLOORS: AtomicU64 = AtomicU64::new(0);
    pub(super) static FLOORS_N: AtomicU64 = AtomicU64::new(0);
    pub(super) static FLOORS_CELLS: AtomicU64 = AtomicU64::new(0);
    pub(super) static FLOORS_STARTS: AtomicU64 = AtomicU64::new(0);
    pub(super) static BLOCK_AT: AtomicU64 = AtomicU64::new(0);
    pub(super) static BLOCK_AT_MISS: AtomicU64 = AtomicU64::new(0);
    pub(super) static BAND_N: AtomicU64 = AtomicU64::new(0);
    pub(super) static BAND_CELLS: AtomicU64 = AtomicU64::new(0);
    pub(super) static BAND_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static BAND_NEW: AtomicU64 = AtomicU64::new(0);
    pub(super) static BAND_RANGE_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static BAND_SEC_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static BAND_COLS_MISS: AtomicU64 = AtomicU64::new(0);
    pub(super) static EPOCH_BUMPS: AtomicU64 = AtomicU64::new(0);
    pub(super) static N: AtomicU64 = AtomicU64::new(0);
    pub(super) fn enabled() -> bool {
        crate::trace_env::env_flag!("MC_SPINE_CHUNK_PROFILE")
    }
    pub(super) fn add(slot: &AtomicU64, t: std::time::Instant) {
        slot.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
    /// A work snapshot, for the deterministic bench: these counters are load-independent,
    /// where the bench's milliseconds are not (the same build measured 30.1 and 67.0
    /// ms/chunk on this host within an hour, tracking the other lanes' load).
    #[cfg(test)]
    pub(crate) fn counters() -> [u64; 14] {
        [
            N.load(Ordering::Relaxed),
            DATA.load(Ordering::Relaxed),
            SECTION.load(Ordering::Relaxed),
            LIGHT.load(Ordering::Relaxed),
            BAND_NEW.load(Ordering::Relaxed),
            BAND_CELLS.load(Ordering::Relaxed),
            BLOCK_AT.load(Ordering::Relaxed),
            BLOCK_AT_MISS.load(Ordering::Relaxed),
            SECTION_N.load(Ordering::Relaxed),
            ENCODE.load(Ordering::Relaxed),
            HIDDEN_N.load(Ordering::Relaxed),
            EXPOSURE_READS.load(Ordering::Relaxed),
            EXPOSURE_READ_BASE.load(Ordering::Relaxed),
            TERRAIN_N.load(Ordering::Relaxed),
        ]
    }

    pub(super) fn report() {
        let n = N.load(Ordering::Relaxed).max(1);
        let per = |v: &AtomicU64| v.load(Ordering::Relaxed) as f64 / 1e6 / n as f64;
        eprintln!(
            "CHUNKPROF n={n} map={:.1}ms data={:.1} section={:.1} light={:.1} (sky={:.1} block={:.1} floors={:.1} x{:.1} block_at={:.0} miss={:.0})",
            per(&MAP),
            per(&DATA),
            per(&SECTION),
            per(&LIGHT),
            per(&SKY),
            per(&BLOCKLIGHT),
            per(&FLOORS),
            FLOORS_N.load(Ordering::Relaxed) as f64 / n as f64,
            BLOCK_AT.load(Ordering::Relaxed) as f64 / n as f64,
            BLOCK_AT_MISS.load(Ordering::Relaxed) as f64 / n as f64
        );
        eprintln!(
            "CHUNKPROF   band lookups={:.0} new={:.1} build={:.2}ms cells={:.0} (per new={:.2}ms)",
            BAND_N.load(Ordering::Relaxed) as f64 / n as f64,
            BAND_NEW.load(Ordering::Relaxed) as f64 / n as f64,
            BAND_NS.load(Ordering::Relaxed) as f64 / 1e6 / n as f64,
            BAND_CELLS.load(Ordering::Relaxed) as f64 / n as f64,
            BAND_NS.load(Ordering::Relaxed) as f64
                / 1e6
                / BAND_NEW.load(Ordering::Relaxed).max(1) as f64
        );
        let new = BAND_NEW.load(Ordering::Relaxed).max(1) as f64;
        eprintln!(
            "CHUNKPROF   read parts/chunk: band_lookup={:.2}ms overlay_read={:.2}ms base_state n={:.0} {:.2}ms",
            BAND_LOOKUP_NS.load(Ordering::Relaxed) as f64 / 1e6 / n as f64,
            OVERLAY_READ_NS.load(Ordering::Relaxed) as f64 / 1e6 / n as f64,
            BASE_STATE_N.load(Ordering::Relaxed) as f64 / n as f64,
            BASE_STATE_NS.load(Ordering::Relaxed) as f64 / 1e6 / n as f64
        );
        eprintln!(
            "CHUNKPROF   read/chunk: n={:.0} ms={:.2} band_path n={:.0} ms={:.2}",
            EXPOSURE_READS.load(Ordering::Relaxed) as f64 / n as f64,
            READ_NS.load(Ordering::Relaxed) as f64 / 1e6 / n as f64,
            READ_BAND_N.load(Ordering::Relaxed) as f64 / n as f64,
            READ_BAND_NS.load(Ordering::Relaxed) as f64 / 1e6 / n as f64
        );
        eprintln!(
            "CHUNKPROF   encode/chunk: hidden={:.0} reads={:.0} base_state={:.0}",
            HIDDEN_N.load(Ordering::Relaxed) as f64 / n as f64,
            EXPOSURE_READS.load(Ordering::Relaxed) as f64 / n as f64,
            EXPOSURE_READ_BASE.load(Ordering::Relaxed) as f64 / n as f64
        );
        eprintln!(
            "CHUNKPROF   phases/chunk: overlay_copy={:.2}ms biomes={:.2}ms fastpath={:.2}ms x{:.1} band_install={:.2}ms encode={:.2}ms section_encode={:.2}ms",
            per(&OVERLAY_APPLY),
            per(&BIOMES),
            per(&FASTPATH),
            FASTPATH_N.load(Ordering::Relaxed) as f64 / n as f64,
            per(&BAND_INSTALL),
            per(&ENCODE),
            per(&ENC_SECTION)
        );
        eprintln!(
            "CHUNKPROF   relight n={:.1}/chunk time={:.1}ms/chunk",
            RE_LIGHT_N.load(Ordering::Relaxed) as f64 / n as f64,
            RE_LIGHT.load(Ordering::Relaxed) as f64 / 1e6 / n as f64
        );
        eprintln!(
            "CHUNKPROF   floors: calls={:.2}/chunk cells={:.0}/chunk starts={:.2}ms/chunk",
            FLOORS_N.load(Ordering::Relaxed) as f64 / n as f64,
            FLOORS_CELLS.load(Ordering::Relaxed) as f64 / n as f64,
            FLOORS_STARTS.load(Ordering::Relaxed) as f64 / 1e6 / n as f64
        );
        eprintln!(
            "CHUNKPROF   band parts: range={:.2}ms sections={:.2}ms cols_miss={:.1} epoch_bumps={:.2}/chunk",
            BAND_RANGE_NS.load(Ordering::Relaxed) as f64 / 1e6 / new,
            BAND_SEC_NS.load(Ordering::Relaxed) as f64 / 1e6 / new,
            BAND_COLS_MISS.load(Ordering::Relaxed) as f64 / n as f64,
            EPOCH_BUMPS.load(Ordering::Relaxed) as f64 / n as f64
        );
    }
}

pub(crate) mod tickprof {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static NS: OnceLock<Mutex<Vec<(&'static str, u64)>>> = OnceLock::new();
    pub(super) static TICKS: AtomicU64 = AtomicU64::new(0);
    pub(super) static HOT_N: AtomicU64 = AtomicU64::new(0);
    pub(super) static FORCED_N: AtomicU64 = AtomicU64::new(0);
    pub(super) static GEN_TICKS: AtomicU64 = AtomicU64::new(0);
    pub(super) static PLATE_SPOTS: AtomicU64 = AtomicU64::new(0);
    pub(super) static PLATE_ON: AtomicU64 = AtomicU64::new(0);
    pub(super) static PLATE_OFF: AtomicU64 = AtomicU64::new(0);
    pub(crate) fn enabled() -> bool {
        crate::trace_env::env_flag!("MC_SPINE_TICK_PROFILE")
    }
    pub(crate) fn add(name: &'static str, elapsed: std::time::Duration) {
        let v = NS.get_or_init(|| Mutex::new(Vec::new()));
        let mut v = v.lock().unwrap();
        match v.iter_mut().find(|(n, _)| *n == name) {
            Some(e) => e.1 += elapsed.as_nanos() as u64,
            None => v.push((name, elapsed.as_nanos() as u64)),
        }
    }
    /// Print the last window and reset it. Averaging "since process start" mixed idle
    /// ticks (0.04 ms) with loaded ones and understated the split by an order of
    /// magnitude.
    pub(super) fn report() {
        let n = TICKS.swap(0, Ordering::Relaxed).max(1) as f64;
        let v = NS.get_or_init(|| Mutex::new(Vec::new()));
        let mut rows: Vec<(&str, f64)> = {
            let mut v = v.lock().unwrap();
            let rows = v
                .iter()
                .map(|(name, ns)| (*name, *ns as f64 / 1e6 / n))
                .collect();
            for e in v.iter_mut() {
                e.1 = 0;
            }
            rows
        };
        rows.sort_by(|a, b| b.1.total_cmp(&a.1));
        let total: f64 = rows.iter().map(|(_, ms)| ms).sum();
        eprintln!("TICKPROF ticks={n} body={total:.2}ms/tick");
        for (name, ms) in rows.iter().take(20) {
            if *ms > 0.001 {
                eprintln!("TICKPROF   {ms:7.3}  {name}");
            }
        }
        eprintln!(
            "TICKPROF   counters: hot/tick={:.0} forceloaded/tick={:.0} gen_leaf_ticks/tick={:.0} spots/tick={:.1} plate_on={:.2} plate_off={:.2}",
            HOT_N.load(Ordering::Relaxed) as f64 / n,
            FORCED_N.load(Ordering::Relaxed) as f64 / n,
            GEN_TICKS.load(Ordering::Relaxed) as f64 / n,
            PLATE_SPOTS.load(Ordering::Relaxed) as f64 / n,
            PLATE_ON.load(Ordering::Relaxed) as f64 / n,
            PLATE_OFF.load(Ordering::Relaxed) as f64 / n
        );
    }
}

/// What the tick loop's wall time is actually spent on: the body, and the sleep between
/// bodies. A thread that burns CPU with a cheap body is spinning, and this is what says so.
pub(crate) mod loopprof {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub(super) static SLEEP_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static BODY_NS: AtomicU64 = AtomicU64::new(0);
    pub(super) static BODY_N: AtomicU64 = AtomicU64::new(0);
    pub(super) fn report() {
        let n = BODY_N.swap(0, Ordering::Relaxed).max(1) as f64;
        let sleep = SLEEP_NS.swap(0, Ordering::Relaxed) as f64 / 1e6 / n;
        let body = BODY_NS.swap(0, Ordering::Relaxed) as f64 / 1e6 / n;
        eprintln!("LOOPPROF ticks={n} body={body:.3}ms sleep={sleep:.3}ms per tick");
    }
}

// ----------------------------------------------------- live runtime wiring
//
// The pieces below are live-server only: `main` installs the background save
// worker, arms the LAN announcer and feeds the load smoother once the tick
// loop is running. Every helper degrades to the exact pre-wiring behaviour in
// tests and tool modes — no worker means the synchronous `save_world` path,
// stage 0 means `LoadBudget::FULL`, which is byte-for-byte what the tick did
// before this block existed.

static LIVE_BALANCER: OnceLock<Mutex<AutoLoadBalancer>> = OnceLock::new();
/// The stage other threads read without taking the balancer's lock. Written
/// once per tick by `observe_live_tick`, read by every budget consumer.
static LIVE_BUDGET_STAGE: AtomicU8 = AtomicU8::new(0);

fn live_balancer() -> &'static Mutex<AutoLoadBalancer> {
    LIVE_BALANCER.get_or_init(|| Mutex::new(AutoLoadBalancer::new()))
}

/// Feed the smoother one measured tick duration and publish the stage it
/// settles on. The tick loop is the only caller outside tests.
fn observe_live_tick(work: Duration) -> LoadBudget {
    let mut balancer = live_balancer().lock().unwrap();
    let budget = balancer.observe(work);
    LIVE_BUDGET_STAGE.store(balancer.stage(), Ordering::Relaxed);
    budget
}

/// The work budget currently in force: `LoadBudget::FULL` whenever the rolling
/// mean tick is healthy, the smoother's tighter stage under real overload.
fn current_load_budget() -> LoadBudget {
    LOAD_BUDGETS[(LIVE_BUDGET_STAGE.load(Ordering::Relaxed).min(LOAD_STAGE_MAX)) as usize]
}

#[cfg(test)]
fn reset_live_load_balancer() {
    *live_balancer().lock().unwrap() = AutoLoadBalancer::new();
    LIVE_BUDGET_STAGE.store(0, Ordering::Relaxed);
    *MOB_SPAWN_CREDIT.lock().unwrap() = [0.0; 6];
}

/// `MC_SPINE_MAX_PLAYERS` (default 20) — the same read the status reply uses.
fn operator_max_players() -> usize {
    std::env::var("MC_SPINE_MAX_PLAYERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
}

/// One shared read of everything the `status` report needs: the smoother's
/// rolling tick mean, the online list, the serviced (hot) chunk set and live
/// process memory. Safe to call from console/RCON while the tick loop runs:
/// the balancer and the region are each taken only inside their own lock.
fn operator_health_snapshot() -> OperatorHealthSnapshot {
    let mspt_ms = live_balancer().lock().unwrap().mspt();
    let players = client_snapshot();
    let loaded_chunks = region().lock().unwrap().hot.len();
    OperatorHealthSnapshot {
        mspt_ms,
        tps: tps_of(mspt_ms),
        players_online: players.len(),
        players_max: operator_max_players(),
        player_names: players.iter().map(|c| c.name.clone()).collect(),
        loaded_chunks,
        memory_mb: None,
        native_mods: 0,
        compat_mods: 0,
    }
}

/// One background writer for the live server's saves. Only `main` installs it;
/// tests and tool modes keep `live_save_worker()` `None` and save synchronously.
static LIVE_SAVE_WORKER: OnceLock<AsyncSaveWorker> = OnceLock::new();

fn live_save_worker() -> Option<&'static AsyncSaveWorker> {
    LIVE_SAVE_WORKER.get()
}

/// `MC_SPINE_BACKUP_MAX_KEEP`: how many `backups/world-*.bak` snapshots stay
/// after each rotation. `0` (the default, or an unparseable value) disables
/// the directory entirely.
fn backup_max_keep() -> usize {
    std::env::var("MC_SPINE_BACKUP_MAX_KEEP")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

/// `backups/world-NNNNNNNN.bak`, pruned to `backup_max_keep()`, holding the
/// same protected payloads as the live overlay files. Only written when the
/// operator asked for them; errors are reported but never block the save.
fn rotate_save_backups(dir: &std::path::Path, snapshot: &WorldSaveSnapshot) {
    let keep = backup_max_keep();
    if keep == 0 {
        return;
    }
    let backup_dir = dir.join("backups");
    if let Err(error) =
        rotate_world_backups(&backup_dir, "world", &wrap_world_payload(&snapshot.overworld), keep)
    {
        eprintln!("[spine] world backup rotation failed: {error}");
    }
    for (dim, payload) in &snapshot.companions {
        let Some(payload) = payload else { continue };
        let prefix = dim_overlay_name(*dim)
            .unwrap_or("world_overlay.bin")
            .trim_end_matches(".bin");
        if let Err(error) =
            rotate_world_backups(&backup_dir, prefix, &wrap_world_payload(payload), keep)
        {
            eprintln!("[spine] {prefix} backup rotation failed: {error}");
        }
    }
}

/// Hand the captured snapshot to the save worker. The worker owns a single
/// pending slot, so the protected overlay (the file a crash recovery reads)
/// is the job it keeps; companion dimensions are small enough to write inline
/// while a slow disk still absorbs the main file.
fn submit_world_snapshot(
    worker: &AsyncSaveWorker,
    snapshot: WorldSaveSnapshot,
    dir: &std::path::Path,
) -> Result<(), String> {
    rotate_save_backups(dir, &snapshot);
    let path = dim_overlay_path(dir, DimId::Overworld);
    worker.submit(path, wrap_world_payload(&snapshot.overworld).into());
    let interval = connection_limits::backup_interval();
    for (dim, payload) in snapshot.companions {
        let path = dim_overlay_path(dir, dim);
        match payload {
            Some(payload) => {
                write_world_file(&path, &wrap_world_payload(&payload), interval)?;
            }
            None => remove_world_save(&path)?,
        }
    }
    Ok(())
}

// ---------- opt-in delta WAL (`MC_SPINE_WAL_PATH`) live wiring ----------
//
// When the operator names a WAL file, every committed overlay block write is
// journaled (`DeltaWalRecord::BlockSet`, flushed per frame) so a crash loses
// near-zero seconds instead of a whole autosave window. Boot replays whatever
// the last durable checkpoint did not cover; each confirmed full save then
// compacts the log. With the variable unset every path below costs at most
// one relaxed atomic load and writes nothing.

/// `true` once `dwal_boot_recover_and_arm` parked an open journal in
/// `live_dwal`. An atomic (not "is the slot set") so a test or a failed
/// re-arm can take the append path back to its zero-cost early return.
static DWAL_ON: AtomicBool = AtomicBool::new(false);

fn live_dwal() -> &'static Mutex<Option<DeltaWalJournal>> {
    static JOURNAL: OnceLock<Mutex<Option<DeltaWalJournal>>> = OnceLock::new();
    JOURNAL.get_or_init(|| Mutex::new(None))
}

fn dwal_enabled() -> bool {
    DWAL_ON.load(Ordering::Relaxed)
}

/// `MC_SPINE_WAL_PATH`, screened by the same `..`/NUL/control-character rule
/// every operator-controlled data path goes through. An invalid value is
/// treated like an unset one: the WAL is opt-in, so a bad path must not crash
/// boot — it logs and stays off.
fn dwal_path_from_env() -> Option<std::path::PathBuf> {
    let raw = std::env::var_os("MC_SPINE_WAL_PATH")?;
    if raw.is_empty() {
        return None;
    }
    let path = std::path::PathBuf::from(raw);
    match ops_data_path_checked(&path, "delta WAL") {
        Ok(()) => Some(path),
        Err(error) => {
            eprintln!("[spine] {error} — delta WAL disabled");
            None
        }
    }
}

/// Journal one committed overlay edit. Called from `set_block_if_inner_dim`
/// and `set_block_if_dim_edit`, so it always runs inside the caller's
/// `SAVE_LOCK` hold — the frame order on disk is exactly the order a full
/// save's snapshot sees the edits in. A rejected `expected` write never
/// reaches this; append failures are reported, never fatal.
fn dwal_note_block_set(dim: DimId, x: i32, y: i32, z: i32, state: u32) {
    if !DWAL_ON.load(Ordering::Relaxed) {
        return;
    }
    let mut journal = live_dwal().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(wal) = journal.as_mut() {
        if let Err(error) =
            wal.append_and_flush(&DeltaWalRecord::BlockSet { dim, x, y, z, state })
        {
            eprintln!("[spine] delta WAL append failed: {error}");
        }
    }
}

/// A full save run as a WAL checkpoint. `compact_after_checkpoint` rewrites
/// the log down to a single marker, so it may only run once the covering save
/// is durable *and* the log cannot still gain frames the snapshot missed.
/// Both properties come from one discipline: `SAVE_LOCK` is held from before
/// the capture until the write has landed and the compaction is done, and the
/// journal mutex is taken inside it — the same order every journaled block
/// write uses (`SAVE_LOCK` then journal, via `set_block_if_dim` →
/// `dwal_note_block_set`), so there is no lock inversion and no interleaving
/// window. Writers simply wait out the save; a frame that lands afterwards is
/// newer than the marker and survives.
///
/// This mirrors `save_world_guarded`'s `SAVE_WRITER`/flag discipline — the
/// thread-local guard flag keeps a nested `with_save_guard` inside the
/// capture re-entrant instead of deadlocking on `SAVE_LOCK`.
fn dwal_save_checkpointed(
    write: impl FnOnce(WorldSaveSnapshot) -> Result<(), String>,
) -> Result<(), String> {
    if SAVE_GUARD.with(|g| g.get()) {
        return Err("world save called inside an ownership transition".into());
    }
    let _writer = SAVE_WRITER.lock().unwrap_or_else(|e| e.into_inner());
    let _save = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    SAVE_GUARD.with(|g| g.set(true));
    struct Reset;
    impl std::ops::Drop for Reset {
        fn drop(&mut self) {
            SAVE_GUARD.with(|g| g.set(false));
        }
    }
    let _reset = Reset;
    let snapshot = persistence_boundary("world snapshot panicked", || {
        Ok(WorldSaveSnapshot::capture())
    })
    .map_err(|e| e.to_string())?;
    let mut journal = live_dwal().lock().unwrap_or_else(|e| e.into_inner());
    write(snapshot)?;
    if let Some(wal) = journal.as_mut() {
        let checkpoint_seq = wal.next_seq();
        wal.compact_after_checkpoint(checkpoint_seq)?;
    }
    Ok(())
}

/// The armed-path autosave: the same worker-vs-serial split as
/// `autosave_world`, but the worker arm includes the `flush` that confirms
/// the queued snapshot actually reached disk before the WAL is compacted — a
/// checkpoint that outran its own write would leave a crash with neither the
/// frames nor the save that was supposed to cover them.
fn dwal_autosave_checkpointed() {
    let dir = world_save_dir();
    let result = match live_save_worker() {
        Some(worker) => dwal_save_checkpointed(|snapshot| {
            submit_world_snapshot(worker, snapshot, &dir)?;
            worker.flush().map_err(|e| format!("async save flush: {e}"))
        }),
        None => dwal_save_checkpointed(|snapshot| snapshot.write(&dir)),
    };
    if let Err(error) = result {
        eprintln!("[spine] WORLD SAVE FAILED: {error}");
    }
    live_anvil_save_if_armed();
}

/// The armed-path shutdown save: `flush_live_saves` lands any queued snapshot
/// first, then the checkpointed write + compact runs under the same freeze as
/// the autosave.
fn dwal_shutdown_save_checkpointed() {
    flush_live_saves();
    let dir = world_save_dir();
    if let Err(error) = dwal_save_checkpointed(|snapshot| {
        rotate_save_backups(&dir, &snapshot);
        snapshot.write(&dir)
    }) {
        eprintln!("[spine] WORLD SAVE FAILED: {error}");
    }
    live_anvil_save_if_armed();
}

/// Boot wiring, called once from `main` after `load_world` and the Anvil
/// import so a journaled edit — newer than both stores — wins its cell.
/// Recovered `BlockSet` frames replay through `set_block_dim` while the
/// journal is still disarmed, so replay does not journal itself; the journal
/// then opens on the same file (continuing sequence numbers past whatever
/// recovery found) and live writes start appending. A WAL that fails
/// validation or recovery is logged and left off: the overlay save stays the
/// authoritative store and boot continues without delta coverage.
fn dwal_boot_recover_and_arm() {
    let Some(path) = dwal_path_from_env() else {
        return;
    };
    match recover_after_checkpoint(&path) {
        Ok(report) => {
            let mut replayed = 0usize;
            let mut skipped = 0usize;
            for record in &report.records {
                match *record {
                    DeltaWalRecord::BlockSet { dim, x, y, z, state } => {
                        set_block_dim(dim, x, y, z, state);
                        replayed += 1;
                    }
                    _ => skipped += 1,
                }
            }
            eprintln!(
                "[spine] delta WAL {}: replayed {replayed} block edit(s){}",
                path.display(),
                if skipped > 0 {
                    format!(", skipped {skipped} non-block record(s)")
                } else {
                    String::new()
                },
            );
            if report.truncated_tail_bytes > 0 {
                eprintln!(
                    "[spine] delta WAL: dropped {} torn tail byte(s)",
                    report.truncated_tail_bytes
                );
            }
        }
        Err(error) => eprintln!("[spine] delta WAL recovery failed: {error}"),
    }
    match DeltaWalJournal::open(&path) {
        Ok(wal) => {
            *live_dwal().lock().unwrap_or_else(|e| e.into_inner()) = Some(wal);
            DWAL_ON.store(true, Ordering::Relaxed);
            eprintln!("[spine] delta WAL armed at {}", path.display());
        }
        Err(error) => eprintln!("[spine] delta WAL disabled: {error}"),
    }
}

/// The autosave: identical to `save_world` until the live worker exists, then
/// a non-blocking submit. The worker's one-slot queue coalesces slow writes so
/// a 20 TPS loop never waits on disk.
fn autosave_world() {
    if dwal_enabled() {
        dwal_autosave_checkpointed();
        return;
    }
    let Some(worker) = live_save_worker() else {
        save_world();
        live_anvil_save_if_armed();
        return;
    };
    let dir = world_save_dir();
    if let Err(error) = save_world_guarded(|snapshot| submit_world_snapshot(worker, snapshot, &dir)) {
        eprintln!("[spine] WORLD SAVE FAILED: {error}");
    }
    live_anvil_save_if_armed();
}

/// Wait until the background writer has finished every snapshot handed to it.
/// No-op for tests and tool modes, which never queue one.
fn flush_live_saves() {
    if let Some(worker) = live_save_worker() {
        if let Err(error) = worker.flush() {
            eprintln!("[spine] WORLD SAVE FAILED: {error}");
        }
    }
}

/// The graceful-stop save: synchronously and complete. Any queued snapshot is
/// written first (the worker may be mid-write), backups rotate, then the final
/// overlay files land before the process is allowed to exit.
fn shutdown_world_save() {
    if dwal_enabled() {
        dwal_shutdown_save_checkpointed();
        return;
    }
    flush_live_saves();
    let dir = world_save_dir();
    if let Err(error) = save_world_guarded(|snapshot| {
        rotate_save_backups(&dir, &snapshot);
        snapshot.write(&dir)
    }) {
        eprintln!("[spine] WORLD SAVE FAILED: {error}");
    }
    live_anvil_save_if_armed();
}

/// Per-natural-spawn-pass fractional credit. `mob_spawn_scale` below 1.0 means
/// "run this pass on a fraction of its turns"; carrying the remainder in a
/// credit keeps the long-run rate exactly the scale instead of rounding every
/// pass off (0.5 would never spawn) or on (0.15 would always spawn).
static MOB_SPAWN_CREDIT: Mutex<[f64; 6]> = Mutex::new([0.0; 6]);

fn mob_spawn_permitted(slot: usize, scale: f64) -> bool {
    if scale >= 1.0 {
        return true;
    }
    let mut credit = MOB_SPAWN_CREDIT.lock().unwrap();
    credit[slot] += scale;
    if credit[slot] >= 1.0 {
        credit[slot] -= 1.0;
        true
    } else {
        false
    }
}

/// The opt-in "Open to LAN" advertiser. `main` arms it only after the game
/// listener owns a port; `stop` drops the multicast thread on shutdown.
static LAN_ANNOUNCER: Mutex<Option<LanAnnouncer>> = Mutex::new(None);

fn start_lan_announcer(port: u16) {
    if std::env::var("MC_SPINE_LAN_BROADCAST").as_deref() != Ok("1") {
        return;
    }
    let motd = std::env::var("MC_SPINE_MOTD").unwrap_or_else(|_| "mc-spine".into());
    match LanAnnouncer::start(&motd, port) {
        Ok(announcer) => {
            *LAN_ANNOUNCER.lock().unwrap() = Some(announcer);
            eprintln!("[lan] advertising \"{motd}\" on port {port}");
        }
        Err(error) => eprintln!("[lan] multicast announcement unavailable: {error}"),
    }
}

fn stop_lan_announcer() {
    if let Some(announcer) = LAN_ANNOUNCER.lock().unwrap().take() {
        announcer.stop();
    }
}

/// The opt-in GS4 query responder ("enable-query" in vanilla
/// server.properties): one UDP thread serving challenge tokens and the live
/// status/rules snapshot. Armed by `MC_SPINE_ENABLE_QUERY=1` (or `true`) or a
/// `enable-query=true` line in server.properties; `MC_SPINE_QUERY_PORT`, then
/// a `query.port` property, then the game port itself select the UDP port —
/// vanilla's own default.
static GS4_QUERY: Mutex<Option<Gs4QueryHandle>> = Mutex::new(None);

struct Gs4QueryHandle {
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl std::ops::Drop for Gs4QueryHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn server_properties_lookup(key: &str) -> Option<String> {
    server_properties_read(&server_properties_path(&server_config_path()))
        .and_then(|props| props.get(key).cloned())
}

fn gs4_query_enabled() -> bool {
    match std::env::var("MC_SPINE_ENABLE_QUERY").as_deref() {
        Ok(v) => return v == "1" || v.eq_ignore_ascii_case("true"),
        Err(_) => {}
    }
    server_properties_lookup("enable-query").is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn gs4_query_port(game_port: u16) -> u16 {
    if let Some(port) = std::env::var("MC_SPINE_QUERY_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return port;
    }
    server_properties_lookup("query.port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(game_port)
}

fn start_gs4_query(bind: &str, game_port: u16) {
    if !gs4_query_enabled() {
        return;
    }
    let port = gs4_query_port(game_port);
    let host_ip = bind.to_string();
    let snapshot_fn: Arc<dyn Fn() -> Gs4StatusSnapshot + Send + Sync> = Arc::new(move || {
        let players = client_snapshot()
            .iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>();
        let motd = std::env::var("MC_SPINE_MOTD").unwrap_or_else(|_| "mc-spine".into());
        let map = world_save_dir()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "world".to_string());
        gs4_vanilla_snapshot(motd, map, players, operator_max_players(), game_port, host_ip.clone())
    });
    let stop = Arc::new(AtomicBool::new(false));
    match spawn_gs4_query_thread(&format!("{bind}:{port}"), snapshot_fn, stop.clone()) {
        Ok((addr, join)) => {
            *GS4_QUERY.lock().unwrap() = Some(Gs4QueryHandle {
                stop,
                join: Some(join),
            });
            eprintln!("[query] GS4 query listening on {addr}");
        }
        Err(error) => eprintln!("[query] GS4 query bind {bind}:{port} failed: {error}"),
    }
}

// Only the Unix signal-handler block calls this; on Windows the stdin `/stop`
// console path exits the process, which ends the responder thread with it.
#[cfg_attr(not(unix), allow(dead_code))]
fn stop_gs4_query() {
    // Dropping the handle flips the flag and joins the responder thread.
    GS4_QUERY.lock().unwrap().take();
}

/// Live Anvil world store: `MC_SPINE_ANVIL_WORLD_DIR` points the running
/// server at a vanilla-format world directory. The boot import runs after the
/// overlay save loads so Anvil data wins on overlap; every autosave and the
/// shutdown save write the world back through the region codec.
fn live_anvil_load_if_armed() {
    let Some(dir) = anvil_world_dir_from_env() else {
        return;
    };
    match live_anvil_load_dir(&dir) {
        Ok(summary) => eprintln!("[anvil] loaded {}: {summary:?}", dir.display()),
        Err(error) => eprintln!("[anvil] load of {} failed: {error}", dir.display()),
    }
}

fn live_anvil_save_if_armed() {
    let Some(dir) = anvil_world_dir_from_env() else {
        return;
    };
    match live_anvil_save_dir(&dir) {
        Ok(summary) => eprintln!("[anvil] saved {}: {summary:?}", dir.display()),
        Err(error) => eprintln!("[anvil] save to {} failed: {error}", dir.display()),
    }
}

/// The dry-ground search is opt-in (`MC_SPINE_SAFE_SPAWN=1`) or automatic on a
/// world that has never been saved when the vanilla datapack generator is
/// running — a fresh vanilla world can put (0, 0) under an ocean. The flat
/// generator and wire tests keep their fixed spawn unless the flag says so.
fn safe_spawn_search_enabled(fresh_world: bool) -> bool {
    std::env::var("MC_SPINE_SAFE_SPAWN").as_deref() == Ok("1")
        || (fresh_world && worldgen::enabled())
}

/// Dry solid ground, two air cells of headroom, away from fluids and contact
/// damage — `find_safe_world_spawn` over the Overworld.
fn compute_safe_initial_world_spawn() -> (f64, f64, f64) {
    find_safe_world_spawn(DimId::Overworld)
}

/// A beacon pulse granted or refreshed on one client: survival/adventure and
/// alive only, an equal-or-stronger entry already counting past the new
/// duration is left alone, and the 0x76 notification reaches the player and
/// their trackers — the same wire shape `live_mob_player_effect` sends.
fn w_be_grant_effect(client: &Client, id: i32, amp: i32, game_ticks: i32) {
    if !matches!(*client.gm.lock().unwrap_or_else(|e| e.into_inner()), 0 | 2)
        || *client.hp.lock().unwrap_or_else(|e| e.into_inner()) <= 0.0
    {
        return;
    }
    let ticks = server_ticks(game_ticks.max(0) as u64) as i32;
    let mut effects = client.effs.lock().unwrap_or_else(|e| e.into_inner());
    if effects
        .iter()
        .any(|effect| effect.id == id && effect.amp >= amp && effect.left >= ticks)
    {
        return;
    }
    effects.retain(|effect| effect.id != id);
    effects.push(Eff {
        id,
        amp,
        left: ticks,
    });
    drop(effects);
    let packet = entity_effect_pkt(client.eid, id, amp, ticks);
    if let Ok(mut sock) = client.sock.lock() {
        let _ = send_packet(&mut sock, 0x76, packet.clone());
    }
    broadcast_seen_ex(client.eid, client.eid, |_| (0x76, packet.clone()));
}

/// The `block_entities` stage of `game_tick`: advances every registered W-BE
/// block entity (campfires, brewing stands, mob spawners, jukeboxes and
/// beacons) and commits the emitted effects — cooked items and ejected glass
/// bottles as live drops, spawner attempts as live mobs, beacon pulses as
/// refreshed player effects. The registry check keeps an untouched world at
/// zero cost: no lock beyond the emptiness read, no RNG draw, no payload
/// drift — `the_tick_preserves_the_saved_world_payload_byte_for_byte` holds.
fn world_block_entity_tick() {
    // One registry scan per tick: the dimensions that actually hold W-BE
    // entities. Empty world (the payload-parity case) pays a single lock and
    // no RNG draw; a world with entities ticks only the dimensions present.
    let dims: Vec<DimId> = {
        let store = world_block_entity_registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut dims: Vec<DimId> = store.keys().map(|(dim, ..)| *dim).collect();
        dims.sort_unstable();
        dims.dedup();
        dims
    };
    if dims.is_empty() {
        return;
    }
    with_save_guard(|| {
        let clients = client_snapshot();
        let world_tick = TICK_IDX.load(Ordering::Relaxed);
        for dim in dims {
            // The beacon event's `player` indexes this slice, so the client
            // list and the position list must come from one filtered pass in
            // the same order.
            let players: Vec<Arc<Client>> = clients
                .iter()
                .filter(|c| {
                    !c.is_spectator()
                        && *c.dim.lock().unwrap_or_else(|e| e.into_inner()) == dim
                })
                .cloned()
                .collect();
            let positions: Vec<[f64; 3]> = players
                .iter()
                .map(|c| {
                    let p = *c.pos.lock().unwrap_or_else(|e| e.into_inner());
                    [p.0, p.1, p.2]
                })
                .collect();
            let events = tick_world_block_entities(
                dim,
                world_tick,
                &positions,
                crate::world_rng::next_u64(),
            );
            for event in events {
                match event {
                    WorldBlockEntityEvent::Cooked { pos, item, .. } => {
                        // Item entities currently live in the Overworld
                        // simulation — the bound `remove_live_brewing_stand`
                        // already drops by.
                        if dim == DimId::Overworld {
                            if let Some(item) = name_to_item(&item) {
                                drop_live_stack(
                                    (
                                        f64::from(pos.0) + 0.5,
                                        f64::from(pos.1) + 0.5,
                                        f64::from(pos.2) + 0.5,
                                    ),
                                    (item, 1, Vec::new()),
                                );
                            }
                        }
                    }
                    WorldBlockEntityEvent::Brewed {
                        pos,
                        ejected_glass_bottle,
                    } => {
                        if ejected_glass_bottle && dim == DimId::Overworld {
                            if let Some(item) = name_to_item("glass_bottle") {
                                drop_live_stack(
                                    (
                                        f64::from(pos.0) + 0.5,
                                        f64::from(pos.1) + 0.5,
                                        f64::from(pos.2) + 0.5,
                                    ),
                                    (item, 1, Vec::new()),
                                );
                            }
                        }
                    }
                    WorldBlockEntityEvent::Spawn {
                        entity_id,
                        location,
                        ..
                    } => {
                        if let Some(etype) = name_to_etype(&entity_id) {
                            let mut ms =
                                mobs().lock().unwrap_or_else(|e| e.into_inner());
                            if ms.len() < MOB_CAP {
                                let mut uuid = [0u8; 16];
                                assert!(os_fill_random(&mut uuid), "entity UUID entropy");
                                uuid[6] = (uuid[6] & 15) | 64;
                                uuid[8] = (uuid[8] & 63) | 128;
                                ms.push(Arc::new(Mob {
                                    dim,
                                    eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                                    uuid,
                                    etype,
                                    pos: Mutex::new((
                                        location[0], location[1], location[2], 0.0, 0.0,
                                    )),
                                    hp: Mutex::new(default_mob_hp(&entity_id)),
                                    last_hit: Mutex::new(
                                        Instant::now() - Duration::from_secs(10),
                                    ),
                                    hurt_at: Mutex::new(
                                        Instant::now() - Duration::from_secs(10),
                                    ),
                                    last_hurt_tick: Mutex::new(None),
                                    vel: Mutex::new([0.0; 3]),
                                    fuse: Mutex::new(0),
                                    fire_ticks: Mutex::new(0),
                                    love: Mutex::new(0),
                                    air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
                                }));
                            }
                        }
                    }
                    WorldBlockEntityEvent::BeaconEffect {
                        player,
                        effect,
                        amplifier,
                        duration_ticks,
                        ..
                    } => {
                        let Some(client) = players.get(player) else {
                            continue;
                        };
                        let Some(id) = name_to_effect(&effect) else {
                            continue;
                        };
                        w_be_grant_effect(
                            client,
                            id,
                            i32::from(amplifier),
                            duration_ticks,
                        );
                    }
                }
            }
        }
    });
}

/// `MC_SPINE_AUTO_UNLOAD_TICKS`: the opt-in game-tick interval between live
/// issue-#105 region sweeps. `0`, unset or unparseable disables the stage
/// entirely — the default tick pays one env read and nothing else, so a
/// server that never asks for the sweep keeps an identical tick.
fn auto_unload_period() -> u64 {
    std::env::var("MC_SPINE_AUTO_UNLOAD_TICKS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// The `auto_unload` stage of `game_tick`: every `MC_SPINE_AUTO_UNLOAD_TICKS`
/// ticks, `sweep_unticketed_regions` evicts resident chunks whose ticket
/// lapsed — chunks covered by no connected client's view window, the
/// Overworld spawn ring or `/forceload` — flushing each unload into the Anvil
/// world dir when `MC_SPINE_ANVIL_WORLD_DIR` is armed or parking it in
/// `evicted_regions` for re-hydration otherwise, so a long-running server
/// stops holding every explored chunk's RAM forever.
fn auto_unload_tick() {
    let period = auto_unload_period();
    if period == 0 {
        return;
    }
    let now = TICK_IDX.load(Ordering::Relaxed);
    if !now.is_multiple_of(server_ticks(period)) {
        return;
    }
    let active = live_client_view_chunks();
    let forced = forceloaded()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let dir = anvil_world_dir_from_env();
    let policy = LiveChunkUnloadPolicy {
        max_unticketed_chunks: 256,
        grace_ticks: server_ticks(600),
    };
    match sweep_unticketed_regions(&active, &forced, policy, now, dir.as_deref()) {
        Ok(summary) if summary.chunks_unloaded > 0 => {
            eprintln!("[spine] auto-unload swept {summary:?}");
        }
        Ok(_) => {}
        Err(error) => eprintln!("[spine] auto-unload sweep failed: {error}"),
    }
}

/// The live hotspot profiler fed by every real `game_tick` is the SAME
/// `OnceLock` `/profile` reads (`live_hotspot_profiler` in
/// `teams_and_commands.rs`) — a second instance would record tick timings a
/// command can never see.
fn game_tick(mut step: impl FnMut(&'static str, fn())) {
    // `/tick freeze` skips world ticks unless `/tick step` or `/tick sprint`
    // queued budget; a sprint always runs. The default is unfrozen, so callers
    // (including tests) that never froze see no change.
    if !tick_controller().lock().unwrap().should_run_game_tick() {
        return;
    }
    // Wall-clock cost of the whole tick body, measured once with a monotonic
    // `Instant`: feeds the shared hotspot profiler and the OPS-2
    // lag-compensation envelope. A skipped (frozen) tick records nothing.
    let wire_tick_start = Instant::now();
    let prof = tickprof::enabled();
    let n = tickprof::TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if prof && n % 100 == 0 {
        tickprof::report();
        lockprof::report();
        loopprof::report();
        // The worldgen lock is the other queue the tick thread sits in: a section pass
        // holds it for tens of milliseconds while the tick reads block states.
        crate::worldgen::report_gen_lock();
    }
    let mut run = |name: &'static str, f: fn()| {
        if prof {
            let t = std::time::Instant::now();
            step(name, f);
            tickprof::add(name, t.elapsed());
        } else {
            step(name, f);
        }
    };
    run("environment", environment_time_tick);
    TICK_IDX.fetch_add(1, Ordering::Relaxed);
    run("hot_regions", refresh_hot);
    // Vanilla 1.21.1 `ServerLevel.tick` phase order: scheduled block ticks,
    // fluid ticks, raids, then the chunk-source pass (entities, block entities,
    // `tickChunk` random ticks), then pending block events. The subsystem
    // buckets below follow that layout.
    run("scheduled_blocks", scheduled_block_tick);
    run("redstone", live_redstone_tick);
    run("generated_fluids", generated_fluid_tick);
    run("water", water_tick);
    let lava_period = server_ticks(FLUID_TICK_GT);
    if TICK_IDX.load(Ordering::Relaxed).is_multiple_of(lava_period) {
        run("lava", lava_tick);
    }
    run("villager_raid", villager_raid_tick);
    run("generated_content", materialize_pending_generation);
    run("interest", interest_tick);
    // Vanilla `ChunkMap.tick` evicts chunks whose tickets lapsed; the sweep is
    // opt-in (`MC_SPINE_AUTO_UNLOAD_TICKS`), so the default stage is a no-op.
    run("auto_unload", auto_unload_tick);
    run("drops", drop_tick);
    run("arrows", w1_arrow_tick);
    run("tnt", w1_tnt_tick);
    run("carts", cart_tick);
    run("vehicles", w1_vehicle_tick);
    run("falling", falling_tick);
    run("mobs", mob_tick);
    // `Leashable.tickLeash` runs inside `Entity.tick`; an empty tether table
    // makes the stage a one-lock no-op.
    run("tethers", tether_tick);
    run("furnaces", furnace_tick);
    run("hoppers", hopper_tick);
    run("brewing", live_brewing_tick);
    run("spawners", spawner_tick);
    // The chunk-source pass' block entities: campfire cooks, brewing timers,
    // spawner attempts, jukebox bookkeeping, beacon pulses. The stage itself
    // short-circuits on an empty W-BE registry, so an untouched world pays
    // nothing and keeps the saved payload bit-identical.
    run("block_entities", world_block_entity_tick);
    run("random_ticks", random_tick_pass);
    run("player_menus", live_menu_tick);
    run("plugins", plugin_tick);
    run("actors", actor_tick);
    #[cfg(feature = "neoforge-events")]
    run("neoforge_player_tick", neoforge_player_tick);
    run("block_updates", apply_queued_block_updates);
    run("chest_viewers", chest_viewer_tick);
    #[cfg(feature = "create-offload")]
    run("mod_offload", mod_offload_tick);
    run("live_mods", live_mod_tick);
    let wire_us = wire_tick_start.elapsed().as_micros().min(u128::from(u32::MAX)) as u32;
    live_hotspot_profiler()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .record_tick_us(wire_us);
    ac_note_server_tick_ms((wire_us / 1000) as u32);
}

fn main() {
    // `server.toml` first, while the process is single-threaded: its `[server]` keys are
    // exported as the `MC_SPINE_*` variables everything below already reads.
    // Tool modes (probe arguments, worldgen dumps) never touch it.
    if std::env::args().nth(1).is_none()
        && std::env::var_os("MC_SPINE_WG_DUMP").is_none()
        && std::env::var_os("MC_SPINE_WG_PROFILE").is_none()
    {
        let doc = server_config_boot();
        ac_install(AcConfig::from_doc(&doc, |name| std::env::var(name).ok()));
        refresh_hidden_tags_from_datapacks(std::path::Path::new(DATAPACK_DIRECTORY));
    }
    // Live mod suite (mod-suite feature): scans MC_SPINE_MODS_DIR and arms the
    // operator mod-sync/progression files. Inert unless the feature is built
    // in *and* one of its env vars names a file or directory.
    live_mod_suite_boot();
    // Terrain evaluates `add(final_density, beardifier)` (see `worldgen`), and the
    // structure pipeline owns the starts that feed it. `worldgen` sits below `feature`
    // in `tools/layers_check.sh`, so the edge is a registered function pointer rather
    // than an import.
    feature::register_beard_resolver();
    // The chunk-promotion pass ticks a marked fluid cell (`LevelChunk.postProcessGeneration`),
    // and the fluid machine lives below the pass' layer, so it is registered the same way the
    // beardifier resolver is. Without it the pass would keep skipping marked fluid cells.
    structure_template::register_fluid_ticker(promotion_fluid_tick);
    // A dump request must never fall through to the live server/save path.
    if std::env::var_os("MC_SPINE_WG_DUMP").is_some()
        && std::env::var_os("MC_SPINE_WG_PROFILE").is_none()
    {
        eprintln!("[wgdump] MC_SPINE_WG_DUMP requires MC_SPINE_WG_PROFILE (use 0 for dump only)");
        std::process::exit(2);
    }
    let probe_args: Vec<String> = std::env::args().skip(1).collect();
    if probe_args
        .first()
        .is_some_and(|arg| arg == "--import-anvil")
    {
        // `--import-anvil <vanilla_world_dir>`: read a vanilla 1.21.1 world
        // (level.dat + region/*.mca + DIM-1/DIM1), apply it to the in-memory
        // world, and persist through the normal save path so the next server
        // run boots the imported world.
        assert_eq!(
            probe_args.len(),
            2,
            "usage: --import-anvil <vanilla_world_dir>"
        );
        let report = import_anvil_world(std::path::Path::new(&probe_args[1]))
            .unwrap_or_else(|error| {
                eprintln!("[anvil] import failed: {error}");
                std::process::exit(2);
            });
        for error in &report.chunk_errors {
            eprintln!("[anvil] {error}");
        }
        apply_anvil_import(&report);
        save_world();
        println!(
            "imported {} chunk(s) from {} region file(s), {} decode error(s)",
            report.chunks.len(),
            report.region_files,
            report.chunk_errors.len()
        );
        return;
    }
    if probe_args
        .first()
        .is_some_and(|arg| arg == "--export-anvil")
    {
        // `--export-anvil <out_dir>`: load the saved world, then write the
        // vanilla directory (`level.dat` + `region/*.mca` per dimension).
        assert_eq!(probe_args.len(), 2, "usage: --export-anvil <out_dir>");
        if let Err(error) = load_world() {
            eprintln!("[anvil] export aborted — world save failed to load: {error}");
            std::process::exit(1);
        }
        let report = export_anvil_world(std::path::Path::new(&probe_args[1]))
            .unwrap_or_else(|error| {
                eprintln!("[anvil] export failed: {error}");
                std::process::exit(2);
            });
        println!(
            "exported {} chunk(s) into {} region file(s) (level.dat written: {})",
            report.chunks_written, report.region_files, report.level_written
        );
        return;
    }
    if probe_args
        .first()
        .is_some_and(|arg| arg == "--worldgen-cell")
    {
        // The terrain stage of one cell, with the aquifer's mark flag: the pair a
        // `NoiseBasedChunkGenerator`/`WorldCarver` mark is decided from. `fluid` is
        // `true` when the state carries a fluid and `mark` when
        // `Aquifer.shouldScheduleFluidUpdate()` held after the call that produced it.
        assert_eq!(
            probe_args.len(),
            6,
            "usage: --worldgen-cell <overworld|nether|end> <seed> <x> <y> <z>"
        );
        let preset = worldgen::NoisePreset::parse(&probe_args[1])
            .expect("implemented biome source: overworld, nether or end");
        let seed = probe_args[2].parse::<i64>().expect("signed 64-bit seed") as u64;
        let coordinate = |i: usize| probe_args[i].parse::<i32>().expect("block coordinate");
        let (x, y, z) = (coordinate(3), coordinate(4), coordinate(5));
        let reg = block_registry::breg().expect("block registry");
        let mut generator =
            worldgen::Gen::load_preset(seed, name_to_state, preset).expect("load noise preset");
        let cols = generator.cols(x >> 4, z >> 4).expect("chunk columns");
        let density = generator.block_density(&cols, x, y, z);
        let state = generator.terrain_cell(&cols, x, y, z);
        let mark = generator.aq_schedule_fluid_update();
        // The two questions vanillas fill asks separately: what the aquifer answered, and
        // whether that answer carried `shouldScheduleFluidUpdate`. `fill=` is the answer with
        // the density the fill used (the carver asks with 0.0), so a cell where the surface
        // material wins can still be told apart from one the aquifer never reached.
        let fill = generator.aq_state(x, y, z, density);
        let mark_fill = generator.aq_schedule_fluid_update();
        println!(
            "{} density={density} fill={} mark_fill={mark_fill} mark={mark}",
            reg.name_of(state),
            fill.map_or("null".to_string(), |st| format!(
                "{}/{}(sched={})",
                reg.name_of(st),
                reg.fluid_name(st),
                st
            )),
        );
        return;
    }
    if probe_args
        .first()
        .is_some_and(|arg| arg == "--worldgen-biome")
    {
        assert_eq!(
            probe_args.len(),
            6,
            "usage: --worldgen-biome <overworld|nether|end> <seed> <quart-x> <quart-y> <quart-z>"
        );
        let preset = worldgen::NoisePreset::parse(&probe_args[1])
            .expect("implemented biome source: overworld, nether or end");
        let seed = probe_args[2].parse::<i64>().expect("signed 64-bit seed") as u64;
        let coordinate = |i: usize| probe_args[i].parse::<i32>().expect("quart coordinate");
        let mut generator =
            worldgen::Gen::load_preset(seed, name_to_state, preset).expect("load noise preset");
        let (_, leaf) = generator
            .biome_quart(coordinate(3), coordinate(4), coordinate(5), None)
            .expect("sample biome");
        println!(
            "{}",
            generator.biome_name_of_leaf(leaf).expect("biome identity")
        );
        return;
    }
    if probe_args
        .first()
        .is_some_and(|arg| arg == "--worldgen-template")
    {
        assert!(
            probe_args.len() == 2 || probe_args.len() == 4 || probe_args.len() == 5,
            "usage: --worldgen-template <compressed-nbt-path> [mirror-index rotation-index [processor-list-json-for-air-preview]]"
        );
        let bytes = std::fs::read(&probe_args[1]).expect("read compressed template");
        let (_, root) = nbt::read_gzip(&bytes, 16 * 1024 * 1024).expect("decode template NBT");
        let reg = block_registry::breg().expect("block registry");
        let template = structure_template::Template::load(&root, reg).expect("load template");
        let mirror = structure_transform::Mirror::from_index(
            probe_args
                .get(2)
                .map_or(0, |s| s.parse().expect("mirror index")),
        )
        .expect("mirror index 0..=2");
        let rotation = structure_transform::Rotation::from_index(
            probe_args
                .get(3)
                .map_or(0, |s| s.parse().expect("rotation index")),
        )
        .expect("rotation index 0..=3");
        println!(
            "size={:?} palettes={} entities={}",
            template.size,
            template.palettes.len(),
            template.entities.len()
        );
        let layout = template
            .block_layout(reg)
            .expect("serialize template layout");
        let digest: String = generation_math::sha256(&layout)
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect();
        println!("loaded_layout_sha256={digest}");
        let bounds =
            structure_transform::bounds(template.size, (0, 0, 0), (0, 0, 0), mirror, rotation);
        println!(
            "transformed_bounds={bounds:?} preview_blocks={}",
            template
                .transformed_blocks(0, mirror, rotation, (0, 0, 0), (0, 0, 0))
                .expect("transform template blocks")
                .len()
        );
        if let Some(path) = probe_args.get(4) {
            let bytes = std::fs::read(path).expect("read processor-list JSON");
            let config = jparse(&bytes).expect("processor-list JSON");
            let processors = structure_template::Processor::list(&config, reg)
                .expect("supported template processors and required children");
            let air = reg.checked_state("minecraft:air", &[]).expect("air state");
            let processed = template
                .process_blocks(
                    0,
                    structure_template::Placement {
                        origin: (0, 0, 0),
                        pivot: (0, 0, 0),
                        mirror,
                        rotation,
                    },
                    &processors,
                    reg,
                    |_| air,
                    |p| {
                        generation_math::Lcg::new(
                            generation_math::hash_block_pos(p.0, p.1, p.2) as u64
                        )
                        .next_f32()
                    },
                )
                .expect("process template preview");
            let mut preview = template.clone();
            preview.palettes = vec![processed];
            let digest: String =
                generation_math::sha256(&preview.block_layout(reg).expect("processed layout"))
                    .iter()
                    .map(|v| format!("{v:02x}"))
                    .collect();
            println!("processor_preview_world=minecraft:air random=position_seeded writes=0");
            println!(
                "processed_pre_placement_blocks={} layout_sha256={digest}",
                preview.palettes[0].len()
            );
        }
        for info in &template.entities {
            println!(
                "entity pos={:?} block_pos={:?} nbt_type={}",
                structure_transform::entity(info.pos, mirror, rotation, (0, 0, 0)),
                info.block_pos,
                info.nbt.id()
            );
        }
        return;
    }
    if probe_args
        .first()
        .is_some_and(|arg| arg == "--worldgen-structure-candidates")
    {
        assert_eq!(
            probe_args.len(),
            4,
            "usage: --worldgen-structure-candidates <seed> <chunk-x> <chunk-z>"
        );
        let seed = probe_args[1].parse::<i64>().expect("signed 64-bit seed") as u64;
        let x = probe_args[2].parse::<i32>().expect("chunk x");
        let z = probe_args[3].parse::<i32>().expect("chunk z");
        let mut sets: Vec<_> = worldgen::Gen::dp_files("structure_set")
            .into_iter()
            .collect();
        assert!(!sets.is_empty(), "structure-set datapack unavailable");
        sets.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, bytes) in sets {
            let config = jparse(&bytes).expect("structure-set JSON");
            let p = config.get("placement").expect("structure-set placement");
            let integer = |field| match p.get(field) {
                Some(J::Num(v))
                    if v.fract() == 0.0 && *v >= i32::MIN as f64 && *v <= i32::MAX as f64 =>
                {
                    *v as i32
                }
                _ => panic!("invalid {field} in {name}"),
            };
            match p.get("type").map(J::str) {
                Some("minecraft:random_spread") => {}
                Some("minecraft:concentric_rings") => {
                    let points = structure_placement::concentric_rings(
                        seed,
                        integer("distance"),
                        integer("spread"),
                        integer("count"),
                        |_, _| None,
                    )
                    .expect("valid concentric rings");
                    println!("{name}\t{points:?}\tpreferred_biome_relocation_unchecked=true\tbiome_and_pieces_unchecked=true");
                    continue;
                }
                other => panic!("unsupported placement type {other:?} in {name}"),
            }
            let spread = match p.get("spread_type").map(J::str).unwrap_or("linear") {
                "linear" => structure_placement::Spread::Linear,
                "triangular" => structure_placement::Spread::Triangular,
                other => panic!("unknown spread type {other} in {name}"),
            };
            let method = match p
                .get("frequency_reduction_method")
                .map(J::str)
                .unwrap_or("default")
            {
                "default" => structure_placement::FrequencyReduction::Default,
                "legacy_type_1" => structure_placement::FrequencyReduction::LegacyType1,
                "legacy_type_2" => structure_placement::FrequencyReduction::LegacyType2,
                "legacy_type_3" => structure_placement::FrequencyReduction::LegacyType3,
                other => panic!("unknown frequency method {other} in {name}"),
            };
            let frequency = match p.get("frequency") {
                None => 1.0,
                Some(J::Num(v)) if (0.0..=1.0).contains(v) => *v as f32,
                _ => panic!("invalid placement frequency in {name}"),
            };
            let salt = integer("salt");
            let placement = structure_placement::RandomSpread::new(
                integer("spacing"),
                integer("separation"),
                salt,
                spread,
            )
            .expect("valid random-spread parameters");
            let candidate = placement.candidate(seed, x, z);
            println!("{name}\t{},{}\tfrequency={}\texclusion_unchecked={}\tbiome_and_pieces_unchecked=true", candidate.0, candidate.1,
                method.allows(seed, salt, candidate.0, candidate.1, frequency), p.get("exclusion_zone").is_some());
        }
        return;
    }
    if probe_args
        .first()
        .is_some_and(|arg| arg.starts_with("--fabric-probe") || arg.starts_with("--fabric-trample"))
    {
        #[cfg(feature = "fabric-probe")]
        {
            if let Err(error) = java_compat::run(&probe_args) {
                eprintln!("[fabric-probe] {error}");
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(feature = "fabric-probe"))]
        {
            eprintln!("Rebuild with --features fabric-probe to run the diagnostic");
            std::process::exit(1);
        }
    }

    if probe_args
        .first()
        .is_some_and(|arg| arg == "--paletted-bench")
    {
        run_paletted_bench();
        return;
    }

    // quick profile: MC_SPINE_WG_PROFILE=n generates n^2 chunks then exits
    if let Ok(n) = std::env::var("MC_SPINE_WG_PROFILE") {
        let seed: u64 = std::env::var("MC_SPINE_SEED")
            .ok()
            .map(|v| {
                worldgen::parse_world_seed(&v)
                    .expect("MC_SPINE_SEED must be a signed long or its unsigned bit pattern")
            })
            .unwrap_or(0);
        let preset = std::env::var("MC_SPINE_WG_PRESET").unwrap_or_else(|_| "overworld".into());
        let preset =
            worldgen::NoisePreset::parse(&preset).expect("implemented offline noise preset");
        if preset != worldgen::NoisePreset::Overworld
            && (std::env::var_os("MC_SPINE_WG_LEAF_RUNTIME_DUMP").is_some()
                || std::env::var_os("MC_SPINE_WG_DIAG_LOAD_WORLD").is_some())
        {
            eprintln!("[wg] dimension-specific tick/save diagnostics are not implemented");
            std::process::exit(2);
        }
        if !worldgen::init_preset(seed, name_to_state, preset) {
            eprintln!("[wg] load failed");
            return;
        }
        let n: i32 = n.parse().unwrap_or(4);
        let t0 = Instant::now();
        for cx in 0..n {
            for cz in 0..n {
                let c = worldgen::chunk_cols(cx, cz).unwrap();
                let _ = c.surface(0, 0);
            }
        }
        eprintln!(
            "[wgprof] {}x{} chunks in {:?} ({:.1} ms/chunk)",
            n,
            n,
            t0.elapsed(),
            t0.elapsed().as_secs_f64() * 1000.0 / (n * n) as f64
        );
        {
            let c = worldgen::EVAL_COUNT.swap(0, std::sync::atomic::Ordering::Relaxed);
            if c > 0 {
                eprintln!(
                    "[wgprof] eval_c calls: {} ({:.0} per chunk)",
                    c,
                    c as f64 / (n * n) as f64
                );
            }
        }
        if std::env::var_os("MC_SPINE_DFSTATS").is_some() {
            eprint!("{}", worldgen::df_stats());
        }
        if let Ok(spec) = std::env::var("MC_SPINE_DFDUMP") {
            worldgen::dfdump(&spec);
        }
        if let Ok(spec) = std::env::var("MC_SPINE_BIOME_DUMP") {
            worldgen::biome_dump(&spec);
        }
        if let Ok(spec) = std::env::var("MC_SPINE_HM_DUMP") {
            generation_context::hm_dump(&spec, feature::populate);
        }
        if let Ok(spec) = std::env::var("MC_SPINE_BIOME_AT") {
            worldgen::biome_at_dump(&spec);
        }
        if let Ok(path) = std::env::var("MC_SPINE_WG_DUMP") {
            if std::env::var("MC_SPINE_WG_DIAG_LOAD_WORLD").as_deref() == Ok("1") {
                load_world().expect("load diagnostic world save");
            }
            // Offline block dump for parity diffs: `MC_SPINE_WG_DUMP_N` chunks per
            // side from (0,0), each section in the send layout `(ly<<8)|(lz<<4)|lx`.
            // `tools/carver_diff.py` reads it against a vanilla region file.
            let n: i32 = std::env::var("MC_SPINE_WG_DUMP_N")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(6);
            let ox: i32 = std::env::var("MC_SPINE_WG_DUMP_X")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let oz: i32 = std::env::var("MC_SPINE_WG_DUMP_Z")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            // Which window a dump covers. Default `world` = the **world** window from
            // `dimension_type`: the Nether and the End store 16 sections while their
            // terrain covers 8, and a comparison has to look at the same rectangle on
            // both sides. `noise` reproduces the pre-split 0/128 dump, which is what the
            // existing `nat-4242-nether-*` rows were captured in (and therefore what the
            // reader of those rows expects): `tools/port_triage.py
            // --height-window noise` sets it together with the triage window, so the dump
            // length and the compared window cannot disagree.
            let (lo, hi) = match std::env::var("MC_SPINE_WG_DUMP_WINDOW").as_deref() {
                Err(std::env::VarError::NotPresent) | Ok("world") => {
                    (worldgen::world_min_y(), worldgen::world_max_y())
                }
                Ok("noise") => (worldgen::min_y(), worldgen::max_y()),
                other => {
                    eprintln!(
                        "[wgdump] invalid MC_SPINE_WG_DUMP_WINDOW {other:?}; expected world or noise"
                    );
                    std::process::exit(2);
                }
            };
            let dump_workers = gen_workers();
            let dump_round = (dump_workers > 1).then(crate::feature::starts_round_begin);
            // The owner shares the round's start cache too: it computes the same starts while
            // the feature layer runs, and recomputing them per chunk was the rest of the
            // duplicated cost (measured: 9.72 s with the workers-only sharing against 7.16 s
            // when the owner shared as well, on the 64-chunk full dump).
            if let Some(round) = dump_round {
                let _ = worldgen::with_gen(|g| g.starts_round = Some(round));
            }
            if let Ok(order_path) = std::env::var("MC_SPINE_POP_ORDER") {
                let fail = |message: String| -> ! {
                    eprintln!("[wgdump] population plan: {message}");
                    std::process::exit(2);
                };
                if std::env::var_os("MC_SPINE_POP_REGION").is_some() {
                    fail(
                        "MC_SPINE_POP_ORDER and MC_SPINE_POP_REGION are mutually exclusive".into(),
                    );
                }
                let text = std::fs::read_to_string(&order_path)
                    .unwrap_or_else(|error| fail(format!("{order_path}: {error}")));
                let allow_extra = match std::env::var("MC_SPINE_POP_ORDER_SCOPE").as_deref() {
                    Err(std::env::VarError::NotPresent) | Ok("exact") => false,
                    Ok("recorded") => true,
                    other => fail(format!(
                        "invalid MC_SPINE_POP_ORDER_SCOPE {other:?}; expected exact or recorded"
                    )),
                };
                let plan = worldgen::parse_population_order(&text, ox, oz, n, allow_extra)
                    .unwrap_or_else(|error| fail(error));
                // The terrain stage runs before the plan, not lazily inside it. Vanilla's
                // `SURFACE` precedes `CARVERS`/`FEATURES` for a chunk, and the marks that
                // stage files have to be in the chunk's `postProcessing` bucket *before* that
                // chunk is promoted - and a promotion happens inside the plan, at the end of
                // the populate that completes its 3x3. Building the rows here (on the same
                // scratch generators the dump uses) is also what makes the marks independent
                // of the worker count: they are decided by the terrain pass, not by whichever
                // section the dump happens to ask for first.
                prefetch_terrain_group(&plan.order, lo, hi, dump_workers, dump_round);
                let requested = plan.order.len();
                let executed =
                    worldgen::with_gen(|g| g.populate_plan(plan, feature::populate))
                        .unwrap_or_else(|| fail("generator not initialized".into()));
                if executed != requested {
                    fail(format!("{executed} of {requested} chunks executed; plan overlaps prior population (use MC_SPINE_WG_PROFILE=0)"));
                }
                eprintln!(
                    "[wgdump] population plan {order_path}: {executed} source chunks executed (scope={})",
                    if allow_extra { "recorded" } else { "exact" }
                );
            }
            let mut out: Vec<u8> = Vec::new();
            let t1 = Instant::now();
            // The request box, in the dump's own order (dz outer, dx inner). One group of
            // chunks has its base terrain built in parallel before the serial section loop
            // touches it; the feature pass stays in that loop, in this order.
            let keys: Vec<(i32, i32)> = (0..n)
                .flat_map(|dz| (0..n).map(move |dx| (ox + dx, oz + dz)))
                .collect();
            for chunk_group in keys.chunks(terrain_group(dump_workers)) {
                prefetch_terrain_group(chunk_group, lo, hi, dump_workers, dump_round);
                for &(cx, cz) in chunk_group {
                    let mut sy = lo;
                    while sy <= hi {
                        let arr = worldgen::terrain_section(cx, sy, cz, crate::feature::populate)
                            .unwrap_or_else(|| Box::new([0u32; 4096]));
                        for v in arr.iter() {
                            out.extend_from_slice(&v.to_le_bytes());
                        }
                        sy += 16;
                    }
                }
            }
            if let Some(round) = dump_round {
                let _ = worldgen::with_gen(|g| g.starts_round = None);
                crate::feature::starts_round_end(round);
            }
            std::fs::write(&path, &out).unwrap();
            if let Ok(canon_path) = std::env::var("MC_SPINE_WG_DUMP_CANON") {
                // Canonical state strings, one per non-air cell, for a comparison that can see
                // a property: the binary dump above is numeric and a name-level diff hides a
                // stair facing the wrong way.
                let reg = crate::block_registry::breg().expect("block registry required");
                let mut text = String::new();
                let chunk_span = (hi - lo + 1) as usize * 256;
                for dz in 0..n {
                    for dx in 0..n {
                        let base = ((dz * n + dx) as usize) * chunk_span;
                        let (cx, cz) = (ox + dx, oz + dz);
                        let mut rows: Vec<(i32, i32, i32, String)> = Vec::new();
                        let mut sy = lo;
                        let mut offset = base;
                        while sy <= hi {
                            for ly in 0..16 {
                                let y = sy + ly;
                                for lz in 0..16 {
                                    for lx in 0..16 {
                                        let st = u32::from_le_bytes(
                                            out[offset * 4..offset * 4 + 4].try_into().unwrap(),
                                        );
                                        offset += 1;
                                        if st == 0 {
                                            continue;
                                        }
                                        rows.push((
                                            cx * 16 + lx as i32,
                                            y,
                                            cz * 16 + lz as i32,
                                            reg.canonical_state(st),
                                        ));
                                    }
                                }
                            }
                            sy += 16;
                        }
                        rows.sort();
                        text.push_str(&format!(
                            "# chunk {cx},{cz} min_y {lo} height {} cells {}\n",
                            hi - lo + 1,
                            rows.len()
                        ));
                        for (x, y, z, state) in rows {
                            text.push_str(&format!("{x}\t{y}\t{z}\t{state}\n"));
                        }
                    }
                }
                std::fs::write(&canon_path, text).unwrap();
                eprintln!("[wgdump] canonical states -> {canon_path}");
            }
            if let Ok(tick_path) = std::env::var("MC_SPINE_WG_LEAF_TICKS") {
                let reg =
                    crate::block_registry::breg().expect("block registry required for tick dump");
                let ticks = worldgen::with_gen(|g| {
                    g.generated_leaf_ticks()
                        .iter()
                        .map(|(p, st)| format!("{}\t{}\t{}\t{}\n", p.0, p.1, p.2, reg.name_of(*st)))
                        .collect::<String>()
                })
                .expect("generator required for tick dump");
                std::fs::write(tick_path, ticks).expect("write generated leaf tick dump");
            }
            if let Ok(path) = std::env::var("MC_SPINE_WG_FLUID_TICKS") {
                let ticks = worldgen::take_generated_fluid_ticks()
                    .iter()
                    .map(|(p, id)| format!("{}\t{}\t{}\t{id}\n", p.0, p.1, p.2))
                    .collect::<String>();
                std::fs::write(path, ticks).expect("write generated fluid tick dump");
            }
            if let Ok(runtime_path) = std::env::var("MC_SPINE_WG_LEAF_RUNTIME_DUMP") {
                if std::env::var_os("MC_SPINE_POP_ORDER").is_none() {
                    eprintln!("[wgdump] leaf runtime diagnostic requires MC_SPINE_POP_ORDER");
                    std::process::exit(2);
                }
                let steps: u64 = std::env::var("MC_SPINE_WG_LEAF_RUNTIME_STEPS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .filter(|n| *n > 0 && *n <= 10_000)
                    .expect("leaf runtime diagnostic needs 1..=10000 steps");
                let halo: i32 = std::env::var("MC_SPINE_WG_LEAF_RUNTIME_HALO")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .filter(|n| (0..=3).contains(n))
                    .unwrap_or(0);
                let warm: HashSet<_> = (ox - halo..ox + n + halo)
                    .flat_map(|x| (oz - halo..oz + n + halo).map(move |z| (x, z)))
                    .collect();
                {
                    let mut region = region().lock().unwrap();
                    region.hot = warm.clone();
                    region.warm = warm;
                }
                let fluid_seeds = std::env::var("MC_SPINE_WG_DIAG_FLUID_SEEDS").ok();
                let fluid_native =
                    std::env::var("MC_SPINE_WG_DIAG_FLUID_NATIVE").as_deref() == Ok("1");
                let production_fluid =
                    std::env::var("MC_SPINE_WG_DIAG_PRODUCTION_FLUID").as_deref() == Ok("1");
                let full_tick = std::env::var("MC_SPINE_WG_DIAG_FULL_TICK").as_deref() == Ok("1");
                if full_tick {
                    for x in ox..ox + n {
                        for z in oz..oz + n {
                            forceloaded().lock().unwrap().insert((x, z));
                        }
                    }
                }
                let mut fluid_diagnostic = GeneratedFluidOwner::new();
                if let Some(path) = &fluid_seeds {
                    let input = std::fs::read_to_string(path).expect("read diagnostic fluid seeds");
                    for line in input.lines() {
                        let p: Vec<i32> = line
                            .split_whitespace()
                            .map(|v| v.parse().expect("fluid seed coordinate"))
                            .collect();
                        assert_eq!(p.len(), 3, "fluid seed requires x y z");
                        let state = block_at(p[0], p[1], p[2]);
                        assert!(
                            (80..=111).contains(&state),
                            "fluid seed must be a liquid block"
                        );
                        if fluid_native {
                            fluid_diagnostic.schedule(
                                (p[0], p[1], p[2]),
                                fluid_from_block(state).unwrap(),
                                1,
                                true,
                            );
                        } else {
                            set_block(p[0], p[1], p[2], state);
                        }
                    }
                }
                for i in 1..=steps {
                    if full_tick {
                        game_tick(|_, tick| tick());
                    } else {
                        TICK_IDX.store(i, Ordering::Relaxed);
                        scheduled_block_tick();
                        if production_fluid {
                            generated_fluid_tick();
                        }
                    }
                    if !full_tick && fluid_seeds.is_some() {
                        if fluid_native {
                            let eligible = region_guard().warm.clone();
                            fluid_diagnostic
                                .tick(i as i64, &eligible)
                                .expect("native fluid diagnostic");
                        } else {
                            water_tick();
                            if i.is_multiple_of(server_ticks(FLUID_TICK_GT)) {
                                lava_tick();
                            }
                        }
                    }
                }
                if std::env::var("MC_SPINE_WG_DIAG_SAVE_WORLD").as_deref() == Ok("1") {
                    save_world();
                }
                let edits = world().lock().unwrap();
                for dz in 0..n {
                    for dx in 0..n {
                        if let Some(chunk) = overworld_edits(&edits, ox + dx, oz + dz) {
                            for (&index, &state) in chunk {
                                assert!(
                                    (index as usize) < 24 * 4096,
                                    "overlay outside diagnostic height"
                                );
                                let cell = ((dz * n + dx) as usize * (24 * 4096)) + index as usize;
                                let offset = cell * 4;
                                if offset + 4 <= out.len() {
                                    out[offset..offset + 4].copy_from_slice(&state.to_le_bytes());
                                }
                            }
                        }
                    }
                }
                std::fs::write(runtime_path, &out).expect("write leaf runtime diagnostic dump");
                eprintln!(
                    "[wgdump] runtime diagnostic: {steps} steps; fluid seeds={} native={fluid_native} production_fluid={production_fluid} full_tick={full_tick}",
                    fluid_seeds.is_some()
                );
            }
            eprintln!(
                "[wgdump] {} chunks -> {} ({} bytes) in {:?}",
                n * n,
                path,
                out.len(),
                t1.elapsed()
            );
            // M3 P1a: the load-independent counters for this run, printed after the binary's
            // own phase time so a measurement line and its attribution travel together.
            // Empty unless `MC_SPINE_M3_PROFILE`/`FEAT_PROFILE` is on.
            let m3 = worldgen::m3_report();
            if !m3.is_empty() {
                eprint!("{m3}");
            }
        }
        if let Ok(path) = std::env::var("MC_SPINE_WG_DUMP_HM") {
            let n: i32 = std::env::var("MC_SPINE_WG_DUMP_N")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(6);
            let ox: i32 = std::env::var("MC_SPINE_WG_DUMP_X")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let oz: i32 = std::env::var("MC_SPINE_WG_DUMP_Z")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let terrain_only = std::env::var("MC_SPINE_WG_DUMP_HM_TERRAIN").is_ok();
            crate::generation_context::heightmap_dump(&path, n, ox, oz, terrain_only);
        }
        if std::env::var("WGD2").is_ok() {
            for name in [
                "minecraft:overworld/ridges",
                "minecraft:overworld/ridges_folded",
                "minecraft:overworld/jaggedness",
                "minecraft:overworld/erosion",
                "noise:jagged",
                "noise:ridge",
                "minecraft:overworld/continents",
                "minecraft:overworld/offset",
                "minecraft:overworld/depth",
                "minecraft:overworld/factor",
                "minecraft:overworld/jaggedness",
                "minecraft:overworld/sloped_cheese",
                "minecraft:shift_x",
                "minecraft:overworld/caves/noodle",
                "minecraft:overworld/caves/entrances",
                "minecraft:overworld/caves/pillars",
                "minecraft:overworld/caves/spaghetti_2d",
                "minecraft:overworld/caves/spaghetti_roughness_function",
            ] {
                for x in [0i32, 200, 1000] {
                    eprintln!(
                        "{} x={} -> {:?}",
                        name,
                        x,
                        worldgen::debug_eval(name, x, 0, 0)
                    );
                }
            }
        }
        if std::env::var("WGD3").is_ok() {
            for y in (-60..=80i32).step_by(8) {
                let d = worldgen::density_at(0, y, 0).unwrap_or(0.0);
                eprintln!("y={} d={:.4}", y, d);
            }
        }
        if std::env::var("WGSURF").is_ok() {
            for x in [-256i32, -128, -64, -32, 0, 32, 64, 128, 256, 400] {
                for z in [-256i32, -128, 0, 128, 256, 400] {
                    let c = worldgen::chunk_cols(x >> 4, z >> 4).unwrap();
                    eprintln!(
                        "SURF {x},{z},{}",
                        c.surface((x & 15) as usize, (z & 15) as usize)
                    );
                }
            }
        }
        if std::env::var("WGD").is_ok() {
            for x in [-64i32, -32, 0, 32, 64, 200, 500] {
                let c = worldgen::chunk_cols(x >> 4, 0).unwrap();
                eprintln!(
                    "x={} surf={} d@y60={:.4} d@y70={:.4} d@y0={:.4}",
                    x,
                    c.surface((x & 15) as usize, 0),
                    worldgen::density_in(&c, x, 60, 0),
                    worldgen::density_in(&c, x, 70, 0),
                    worldgen::density_in(&c, x, 0, 0)
                );
            }
        }
        if std::env::var("WGBPROF").is_ok() {
            let t0 = Instant::now();
            for cx in 0..n {
                for cz in 0..n {
                    for sec in 0..24 {
                        let _ = worldgen::biome_section(cx, sec * 16 - 64, cz);
                    }
                }
            }
            eprintln!("[wgbprof] {}x{} chunks x24 sec in {:?}", n, n, t0.elapsed());
        }
        if std::env::var("WGCOL").is_ok() {
            // dump vertical strips of state ids around the surface for sample coords
            for x in [
                -2000i32, -1500, -1200, -900, -640, -500, -200, -64, 0, 64, 200, 400, 900, 1400,
                2000,
            ] {
                for z in [0i32] {
                    let cc = match worldgen::chunk_cols(x >> 4, z >> 4) {
                        Some(c) => c,
                        None => continue,
                    };
                    let surf = cc.surface((x & 15) as usize, (z & 15) as usize);
                    let mut strip = String::new();
                    for y in (surf - 6..=surf + 1)
                        .rev()
                        .chain((-8..=4).rev())
                        .chain((-64..=-58).rev())
                    {
                        let st = wg2_block(x, y, z).unwrap_or(99999);
                        strip.push_str(&format!("{}:{} ", y, st));
                    }
                    eprintln!("COL {},{} surf={} | {}", x, z, surf, strip);
                }
            }
        }
        if std::env::var("WGBIOME").is_ok() {
            // climate parameters and biome at one quart position, seed 123
            if let Some((b, t)) = worldgen::biome_debug(123, 123, 123) {
                eprintln!("biome @123,123,123 = {} params {:?}", b, t);
            }
            const BCH: [&str; 64] = [
                "bd", "bj", "bl", "bc", "bf", "cg", "co", "cf", "df", "do", "dd", "df", "dl", "de",
                "ds", "dc", "eb", "eh", "em", "eb", "ff", "fo", "fo", "fp", "fr", "gr", "is", "jp",
                "jg", "lo", "lc", "ms", "md", "mf", "nw", "oc", "ob", "op", "os", "pl", "rv", "sv",
                "sp", "si", "sb", "sp", "ss", "st", "ss", "sj", "pk", "ss", "sp", "sw", "ta", "te",
                "tv", "wo", "wf", "wf", "wg", "wh", "ws", "wb",
            ];
            let _ = BCH;
            for z in -64..64i32 {
                let mut row = String::new();
                for x in -64..64i32 {
                    let b = worldgen::biome_at(x, 16, z).unwrap_or(-1);
                    row.push(match b {
                        39 => '.',                                       // plains
                        21 | 20 | 4 => 'F',                              // forest/flower/birch
                        54 | 47 | 37 | 38 => 'T',                        // taiga family
                        14 => 'D',                                       // desert
                        41 | 42 => 'S',                                  // savanna
                        35 | 13 | 9 | 11 | 12 | 29 | 22 | 57 | 6 => '~', // oceans
                        40 | 24 => '|',                                  // river
                        3 | 8 => '#',                                    // beach
                        45 | 25 | 23 | 46 | 26 | 50 | 27 => '^',         // snowy/peaks
                        0 | 19 | 63 => 'M',                              // badlands
                        16 | 28 | 49 | 1 => 'J',                         // jungle
                        53 | 31 => 'W',                                  // swamp/mangrove
                        32 => 'm',                                       // meadow
                        5 => 'C',                                        // cherry grove
                        33 => 'u',                                       // mushroom
                        30 | 15 | 10 => '!',                             // lush/dripstone/deepdark
                        _ => '?',
                    });
                }
                eprintln!("{}", row);
            }
        }
        if std::env::var("WGFEAT").is_ok() {
            // scan a few chunks: count feature-layer overrides by state, and
            // print above-surface columns to verify trees/vegetation appear
            use std::collections::HashMap;
            let mut hist: HashMap<u32, usize> = HashMap::new();
            let t0 = std::time::Instant::now();
            let mut n = 0usize;
            for cx in -4..=4 {
                for cz in -4..=4 {
                    for lx in 0..16i32 {
                        for lz in 0..16i32 {
                            let (x, z) = (cx * 16 + lx, cz * 16 + lz);
                            let cc = worldgen::chunk_cols(cx, cz).unwrap();
                            let top = cc.wtop((x & 15) as usize, (z & 15) as usize);
                            for y in top - 2..top + 24 {
                                if let Some(st) =
                                    worldgen::feature_state(x, y, z, feature::populate)
                                {
                                    *hist.entry(st).or_default() += 1;
                                    n += 1;
                                }
                            }
                        }
                    }
                }
            }
            eprintln!("[feat] {} overrides in {:?}", n, t0.elapsed());
            let mut top: Vec<(u32, usize)> = hist.into_iter().collect();
            top.sort_by_key(|e| std::cmp::Reverse(e.1));
            for (st, c) in top.iter().take(25) {
                let nm = crate::block_registry::breg()
                    .map(|r| r.name_of(*st).to_string())
                    .unwrap_or_default();
                eprintln!("[feat]   {} x{} (id {})", nm, c, st);
            }
            // a surface column: find a log and print its column
            'outer: for cx in -4..=4 {
                for cz in -4..=4 {
                    for lx in 0..16i32 {
                        for lz in 0..16i32 {
                            let (x, z) = (cx * 16 + lx, cz * 16 + lz);
                            let cc = worldgen::chunk_cols(cx, cz).unwrap();
                            let top = cc.wtop((x & 15) as usize, (z & 15) as usize);
                            for y in top..top + 20 {
                                let st = wg2_block(x, y, z).unwrap_or(99999);
                                let n = crate::block_registry::breg()
                                    .map(|r| r.name_of(st).to_string())
                                    .unwrap_or_default();
                                if n.contains("log") {
                                    eprintln!("[feat] LOG @{},{} surf={}", x, z, top);
                                    for yy in top - 3..top + 10 {
                                        let nm = crate::block_registry::breg()
                                            .map(|r| {
                                                r.name_of(wg2_block(x, yy, z).unwrap_or(99999))
                                                    .to_string()
                                            })
                                            .unwrap_or_default();
                                        eprintln!("[feat]   y={} {}", yy, nm);
                                    }
                                    break 'outer;
                                }
                            }
                        }
                    }
                }
            }
        }
        // heightmap preview: # mountain, : land, . water-edge, ~ ocean
        if std::env::var("WGMAP").is_ok() {
            for z in -256..256 {
                let mut row = String::new();
                for x in -256..256 {
                    let c = worldgen::chunk_cols(x >> 4, z >> 4).unwrap();
                    let s = c.surface((x & 15) as usize, (z & 15) as usize);
                    row.push(if s > 100 {
                        '#'
                    } else if s > 63 {
                        ':'
                    } else if s > 57 {
                        '.'
                    } else {
                        '~'
                    });
                }
                eprintln!("{}", row);
            }
        }
        return;
    }
    // datapack worldgen uses the saved seed by default. An explicit environment seed may only
    // select a new world's seed or confirm the existing one.
    let requested_seed = std::env::var("MC_SPINE_SEED")
        .ok()
        .map(|v| {
            worldgen::parse_world_seed(&v)
                .expect("MC_SPINE_SEED must be a signed long or its unsigned bit pattern")
        });
    let saved_seed = match load_world_seed_in(&world_save_dir()) {
        Ok(seed) => seed,
        Err(error) => {
            eprintln!("[spine] refusing world save: {error}");
            std::process::exit(1);
        }
    };
    let wg_seed = match resolve_world_seed(requested_seed, saved_seed) {
        Ok(seed) => seed,
        Err(error) => {
            eprintln!("[spine] refusing world start: {error}");
            std::process::exit(1);
        }
    };
    if std::env::var("MC_SPINE_WG").ok().as_deref() != Some("old") {
        if worldgen::init(wg_seed, name_to_state) {
            // The Nether and the End are generated by the same seed with their own noise
            // settings, so they are installed here, once, next to the Overworld: a streamed
            // Nether section must not depend on a diagnostic having loaded the Nether first.
            // `init` keeps the Overworld active, so every dim-less accessor below still means
            // the Overworld.
            for dim in [DimId::Nether, DimId::End] {
                if !worldgen::init_dim(dim, wg_seed, name_to_state) {
                    eprintln!(
                        "[wg] {} generator missing — that dimension cannot be streamed. {}",
                        dim.dimension_id(),
                        worldgen::VANILLA_DATA_MISSING
                    );
                }
            }
            *WORLD_SPAWN.lock().unwrap() = (0.5, worldgen::surface_of(0, 0) as f64 + 0.5, 0.5, 0.0);
            eprintln!("[wg] vanilla datapack worldgen active (seed {})", wg_seed);
            if let Ok(spec) = std::env::var("MC_SPINE_DFDUMP") {
                worldgen::dfdump(&spec);
            }
        } else {
            eprintln!(
                "[wg] worldgen datapack missing — flat generator. {}",
                worldgen::VANILLA_DATA_MISSING
            );
        }
    }
    WORLD_SEED.store(wg_seed, Ordering::Relaxed);
    world_rng::seed(wg_seed);

    // A corrupt protected save must stop startup rather than silently
    // continuing with a world that failed validation.
    let fresh_world = !dim_overlay_path(&world_save_dir(), DimId::Overworld).exists();
    if let Err(error) = load_world() {
        eprintln!("[spine] refusing corrupt world save: {error} (original file preserved)");
        std::process::exit(1);
    }
    // A configured live Anvil directory imports after the overlay save so its
    // chunks win where both stores name the same position.
    live_anvil_load_if_armed();
    // Opt-in delta WAL (`MC_SPINE_WAL_PATH`): replay journaled post-save edits
    // last so the newest recorded write wins, then arm the journal for live
    // edits. A no-op unless the variable names a valid path.
    dwal_boot_recover_and_arm();
    // The initial-spawn decision happens after the save loads so a recorded
    // `setworldspawn` always wins; only a world with nothing recorded (or an
    // explicit MC_SPINE_SAFE_SPAWN=1) takes the dry-ground search.
    if safe_spawn_search_enabled(fresh_world) {
        let (x, y, z) = compute_safe_initial_world_spawn();
        let mut spawn = WORLD_SPAWN.lock().unwrap();
        *spawn = (x, y, z, spawn.3);
        eprintln!("[spine] safe world spawn at {x:.1} {y:.1} {z:.1}");
    }
    if let Err(error) = landing::initialize() {
        eprintln!("[mod-compatibility] {error}");
        std::process::exit(1);
    }
    // Fail before any state is loaded or listener opened.
    let bind = std::env::var("MC_SPINE_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1".into());
    if let Err(why) = connection_limits::check_bind(
        "game listener",
        &bind,
        connection_limits::public_bind_allowed(),
    ) {
        eprintln!("[security] {why}");
        std::process::exit(2);
    }
    if let Err(error) = load_operators(std::path::Path::new(OPS_FILE)) {
        eprintln!("[spine] could not load {OPS_FILE}: {error}");
    }
    if let Err(error) = reload_datapacks(std::path::Path::new(DATAPACK_DIRECTORY), None, true) {
        eprintln!("[spine] {error}");
    }
    std::thread::spawn(rcon_listener);
    http_server();
    // Opt-in debug seeding of a few mobs near spawn (only if none restored
    // from save). Vanilla spawns nothing at startup; a live natural spawner
    // populates the area instead. `MC_SPINE_SEED_DEBUG_MOBS=1` restores the
    // old fixed cow/zombie pair for harnesses that want a known entity set.
    if std::env::var("MC_SPINE_SEED_DEBUG_MOBS").as_deref() == Ok("1")
        && mobs().lock().unwrap().is_empty()
    {
        let mut ms = mobs().lock().unwrap();
        // (etype, x, z): 22=cow, 124=zombie (1.21.1 entity ids)
        for (i, (et, x, z)) in [
            (22i64, 5.5f64, 2.5f64),
            (22, -3.5, 4.5),
            (124, 8.5, 6.5),
            (124, -8.5, -6.5),
        ]
        .iter()
        .enumerate()
        {
            ms.push(Arc::new(Mob {
                dim: DimId::Overworld,
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                uuid: [i as u8; 16],
                etype: *et,
                pos: Mutex::new((*x, surface_y(*x as i32, *z as i32), *z, 0.0, 0.0)),
                hp: Mutex::new(20.0),
                last_hit: Mutex::new(Instant::now() - Duration::from_secs(10)),
                hurt_at: Mutex::new(Instant::now() - Duration::from_secs(10)),
                last_hurt_tick: Mutex::new(None),
                vel: Mutex::new([0.0; 3]),
                fuse: Mutex::new(0),
                fire_ticks: Mutex::new(0),
                love: Mutex::new(0),
                air: Mutex::new(server_ticks(AIR_SUPPLY_GT) as i32),
            }));
        }
    }
    // The live autosave worker: periodic saves hand it serialized snapshots so
    // a slow disk coalesces behind one pending slot instead of stalling the
    // 20 TPS loop. Only the live server installs it; tests and tool modes keep
    // the synchronous save path byte-for-byte.
    let _ = LIVE_SAVE_WORKER.set(AsyncSaveWorker::spawn());
    thread::spawn(|| {
        // The deadline derives from the previous one, not from the sleep
        // duration, so tick work that overruns does not walk the clock later.
        // Under sustained overload the deadline is re-anchored instead of
        // spinning to catch up, which would only add more work.
        const TICK_INTERVAL: Duration = Duration::from_millis(TICK_MS);
        let mut next_tick = Instant::now();
        let mut stats = tick_stats::TickStats::new(next_tick, TICK_INTERVAL);
        loop {
            next_tick += TICK_INTERVAL;
            let __s0 = std::time::Instant::now();
            std::thread::sleep(next_tick.saturating_duration_since(__s0));
            loopprof::SLEEP_NS.fetch_add(__s0.elapsed().as_nanos() as u64, Ordering::Relaxed);
            next_tick = next_deadline(next_tick, Instant::now() > next_tick + TICK_INTERVAL);
            let started = Instant::now();
            loopprof::BODY_N.fetch_add(1, Ordering::Relaxed);
            // A panicking tick must not leave the listener serving a world that no
            // longer advances. Log loudly and exit non-zero so a supervisor
            // (systemd, docker) restarts from the last autosave. The world is not
            // saved here: the panic may have left it half-updated, and a torn state
            // must not replace the last good save. (The panic also poisons every
            // Mutex it held; the lock().unwrap() calls elsewhere are left as they are.)
            if let Err(msg) = connection_limits::catch_panic(|| game_tick(|_, tick| tick())) {
                eprintln!("[FATAL] tick thread panicked: {msg}; exiting so the server restarts");
                std::process::exit(70);
            }
            // the working server's own record of G3: the tick body's cost against
            // the 50ms budget, once per 100 ticks
            loopprof::BODY_NS.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
            // The smoother sees every tick's measured work: full rate while the
            // rolling mean stays under its warn band, smooth throttling of
            // background chunk/mob work only under real overload.
            observe_live_tick(started.elapsed());
            if let Some(r) = stats.record(Instant::now(), started.elapsed()) {
                eprintln!(
                    "[tick] observed_tps={:.2} avg_work_ms={:.2} max_work_ms={:.2} over_budget={}/{}",
                    r.observed_tps, r.mean_work_ms, r.max_work_ms, r.overruns, tick_stats::SAMPLE_TICKS
                );
            }
        }
    });
    // vanilla saves the world on SIGTERM/SIGINT (docker/systemd stop);
    // without a handler the autosave window (30s) is lost on kill.
    // signal-hook is a Unix-only dependency; Windows exits via the stdin
    // `/stop` / SHUTDOWN console loop spawned below instead.
    #[cfg(unix)]
    {
        let mut sigs = signal_hook::iterator::Signals::new([
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGINT,
        ])
        .expect("signal hook");
        thread::spawn(move || {
            if sigs.forever().next().is_some() {
                stop_lan_announcer();
                stop_gs4_query();
                shutdown_world_save();
                disconnect_all_clients("Server closed");
                thread::sleep(Duration::from_millis(100));
                #[cfg(feature = "neoforge-events")]
                if let Some(runtime) = java_compat::NEOFORGE.get() {
                    runtime.cleanup_staged_mods();
                }
                std::process::exit(0);
            }
        });
    }
    // Daemon stdin console (live server mode only — every tool/probe/dump mode
    // has already returned). Lines run as level-4 console commands; `stop` ends
    // the loop (the dispatched `/stop` shuts the process down itself), and EOF
    // exits silently so headless/non-TTY runs are unaffected.
    thread::spawn(|| {
        let stdin = std::io::stdin();
        let _ = run_stdin_console_loop(stdin.lock(), |line| {
            let mut none: Option<&Client> = None;
            run_command(None, &format!("/{line}"), &mut none);
            String::new()
        });
    });
    let port: u16 = std::env::var("MC_SPINE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(25599);
    let l = TcpListener::bind((bind.as_str(), port)).expect("game listener bind failed");
    // Vanilla's "Open to LAN" multicast advertisement — opt-in, and only ever
    // armed on the port the listener actually owns.
    start_lan_announcer(l.local_addr().map(|a| a.port()).unwrap_or(port));
    // The GS4 query responder follows the same rule: it is armed only once
    // the game listener owns its port, and only when opted into.
    start_gs4_query(&bind, l.local_addr().map(|a| a.port()).unwrap_or(port));
    eprintln!(
        "mc-spine v0.6 listening {bind}:{port} (offline identities; no Mojang authentication)"
    );
    if !connection_limits::is_loopback_bind(&bind) {
        eprintln!("[security] non-loopback bind (MC_SPINE_ALLOW_PUBLIC_BIND set): restrict access with a firewall/private network");
    }
    static ACTIVE: AtomicUsize = AtomicUsize::new(0);
    for st in l.incoming() {
        if let Ok(s) = st {
            // connection cap: refuse above 64 live handler threads
            if ACTIVE.fetch_add(1, Ordering::SeqCst) >= 64 {
                ACTIVE.fetch_sub(1, Ordering::SeqCst);
                log_refusal(&peer_label(&s), "server connection cap (64) reached");
                drop(s);
                continue;
            }
            // ...and above the per-IP cap, so one host cannot take all 64 slots
            let ip_slot = match s.peer_addr() {
                Ok(a) => match IP_CONNS.acquire(a.ip(), connection_limits::per_ip_limit(a.ip())) {
                    Some(slot) => Some(slot),
                    None => {
                        ACTIVE.fetch_sub(1, Ordering::SeqCst);
                        log_refusal(&a.ip().to_string(), "per-IP connection limit reached");
                        drop(s);
                        continue;
                    }
                },
                Err(_) => None,
            };
            thread::spawn(move || {
                let _slot = ConnectionSlot(&ACTIVE);
                let _ip_slot = ip_slot;
                handle(s);
            });
        }
    }
}

/// Test-only access to the region snapshot. The production code reads it through
/// `region_guard`; these exist so a fixture can save, force and restore it.
#[cfg(test)]
pub fn test_region() -> RegionSets {
    let r = region().lock().unwrap();
    (r.hot.clone(), r.warm.clone())
}
#[cfg(test)]
pub fn set_test_region(hot: HashSet<(i32, i32)>, warm: HashSet<(i32, i32)>) {
    let mut r = region().lock().unwrap();
    r.hot = hot;
    r.warm = warm;
}
#[cfg(test)]
pub fn set_test_region_warm(warm: HashSet<(i32, i32)>) {
    region().lock().unwrap().warm = warm;
}
#[cfg(test)]
pub fn test_region_warm_insert(c: (i32, i32)) {
    region().lock().unwrap().warm.insert(c);
}
#[cfg(test)]
pub fn test_region_warm_remove(c: &(i32, i32)) {
    region().lock().unwrap().warm.remove(c);
}

// Hold across setup, tick execution and restoration of shared world fixtures.
#[cfg(test)]
static WORLD_TEST_LOCK: Mutex<()> = Mutex::new(());

/// The world lock, taken with any poison cleared. A test that fails while holding it would
/// otherwise poison the mutex, and every later test would fail on its own unwrap - one real failure
/// turned into thirty, which is what a red suite has to make impossible to misread. The lock exists
/// to stop two tests touching the world at once, and a test that has finished cannot do that.
#[cfg(test)]
fn world_guard() -> std::sync::MutexGuard<'static, ()> {
    WORLD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
mod brigadier;
mod brigadier_wire;
#[cfg(test)]
mod brigadier_tests;
#[cfg(test)]
mod brigadier_wire_tests;
#[cfg(test)]
mod command_tests;
#[cfg(test)]
mod container_tests;
#[cfg(test)]
mod craft_tests;
#[cfg(test)]
mod effect_tests;
#[cfg(test)]
mod entity_encoding_tests;
#[cfg(test)]
mod fanout_tests;
mod flowing_fluid;
#[cfg(test)]
mod furnace_cache_tests;
#[cfg(test)]
mod merge_tests;
mod nbt;
#[cfg(test)]
mod persistence_tests;
#[cfg(test)]
mod primitives_tests;
#[cfg(test)]
mod registry_tests;
mod mineshaft_layout;
mod mineshaft;
mod mineshaft_piece;
mod scheduled_ticks;
mod structure_placement;
mod structure_template;
mod structure_transform;
#[cfg(test)]
mod test_support;
mod tick_persistence;
mod tick_queue;
#[cfg(test)]
mod tick_tests;
#[cfg(test)]
mod anticheat_world_tests;
mod generation_payload;
mod generated_content;
mod chest_loot;
mod minecart;
mod spawner;
mod voxel_faces;

#[cfg(test)]
mod resource_pack_url_tests {
    use super::resource_pack_url;
    #[test]
    fn loopback_only_by_default_and_explicit_external_url() {
        assert_eq!(
            resource_pack_url("127.0.0.1", None).as_deref(),
            Some("http://127.0.0.1:25598/pack.zip")
        );
        assert!(resource_pack_url("0.0.0.0", None).is_none());
        assert_eq!(
            resource_pack_url("0.0.0.0", Some("https://example.org/pack.zip")).as_deref(),
            Some("https://example.org/pack.zip")
        );
        assert!(resource_pack_url("0.0.0.0", Some("javascript:alert(1)")).is_none());
    }
}

#[cfg(test)]
mod mineshaft_runtime_tests {
    use super::*;
    use std::net::{Shutdown, TcpListener};

    struct RuntimeSnapshot {
        world: HashMap<DimChunkKey, HashMap<u32, u32>>,
        chests: HashMap<ContainerKey, HashMap<i16, Stack>>,
        pending_block_loot: HashMap<DimBlockPos, PendingBlockLoot>,
        drops: Vec<Drop>,
        mobs: Vec<Arc<Mob>>,
        clients: Vec<Arc<Client>>,
        forced: HashSet<(i32, i32)>,
        generated: generated_content::Content,
        region: RegionSets,
        world_time: u64,
        difficulty: u8,
        tick_idx: u64,
        rng: u64,
        spawn: (f64, f64, f64, f32),
    }

    impl RuntimeSnapshot {
        fn take() -> Self {
            Self {
                world: std::mem::take(&mut *world().lock().unwrap()),
                chests: std::mem::take(&mut *chests().lock().unwrap()),
                pending_block_loot: std::mem::take(&mut *pending_block_loot().lock().unwrap()),
                drops: std::mem::take(&mut *drops().lock().unwrap()),
                mobs: std::mem::take(&mut *mobs().lock().unwrap()),
                clients: std::mem::take(&mut *clients().lock().unwrap()),
                forced: std::mem::take(&mut *forceloaded().lock().unwrap()),
                generated: std::mem::take(&mut *generated_content().lock().unwrap()),
                region: test_region(),
                world_time: WORLD_TIME.load(Ordering::Relaxed),
                difficulty: DIFFICULTY.load(Ordering::Relaxed),
                tick_idx: TICK_IDX.load(Ordering::Relaxed),
                rng: world_rng::saved_state(),
                spawn: *WORLD_SPAWN.lock().unwrap(),
            }
        }
    }

    impl std::ops::Drop for RuntimeSnapshot {
        fn drop(&mut self) {
            *world().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.world);
            *chests().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.chests);
            *pending_block_loot().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.pending_block_loot);
            *drops().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.drops);
            *mobs().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.mobs);
            *clients().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.clients);
            *forceloaded().lock().unwrap_or_else(|e| e.into_inner()) = std::mem::take(&mut self.forced);
            *generated_content().lock().unwrap_or_else(|e| e.into_inner()) =
                std::mem::take(&mut self.generated);
            set_test_region(self.region.0.clone(), self.region.1.clone());
            WORLD_TIME.store(self.world_time, Ordering::Relaxed);
            DIFFICULTY.store(self.difficulty, Ordering::Relaxed);
            TICK_IDX.store(self.tick_idx, Ordering::Relaxed);
            world_rng::restore(self.rng);
            *WORLD_SPAWN.lock().unwrap_or_else(|e| e.into_inner()) = self.spawn;
        }
    }

    fn read_server_packet(s: &mut TcpStream, deadline: Instant) -> Option<(i64, Vec<u8>)> {
        let mut len = 0u64;
        let mut done = false;
        for i in 0..5 {
            let mut b = [0u8; 1];
            read_before(s, &mut b, deadline)?;
            len |= ((b[0] & 0x7f) as u64) << (7 * i);
            if b[0] & 0x80 == 0 {
                done = true;
                break;
            }
        }
        if !done || len == 0 || len > 16 * 1024 * 1024 {
            return None;
        }
        let mut body = vec![0u8; len as usize];
        read_before(s, &mut body, deadline)?;
        let mut sl = &body[..];
        let id = read_varint(&mut sl)?;
        Some((id, sl.to_vec()))
    }

    fn connect_play_client(name: &str, uuid: [u8; 16]) -> (TcpStream, std::thread::JoinHandle<()>, Arc<Client>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = std::thread::spawn(move || {
            let (srv, _) = listener.accept().unwrap();
            handle(srv);
        });
        let mut cli = TcpStream::connect(addr).unwrap();
        cli.set_nodelay(true).ok();
        cli.set_read_timeout(Some(Duration::from_secs(15))).unwrap();

        // Handshake -> Login
        let mut hs = Vec::new();
        write_varint(&mut hs, 767);
        write_str(&mut hs, "127.0.0.1");
        hs.extend_from_slice(&25565u16.to_be_bytes());
        write_varint(&mut hs, 2);
        send_packet(&mut cli, 0x00, hs).unwrap();

        // Login Start
        let mut login = Vec::new();
        write_str(&mut login, name);
        login.extend_from_slice(&uuid);
        send_packet(&mut cli, 0x00, login).unwrap();

        // Read until Login Success (0x02)
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (pid, _) = read_server_packet(&mut cli, deadline).expect("login success");
            if pid == 0x02 {
                break;
            }
        }
        send_packet(&mut cli, 0x03, vec![]).unwrap(); // Login Acknowledged

        // Config: send ClientInformation with view=2 so chunk streaming stays small,
        // then answer Select Known Packs (0x0e) and Finish Configuration (0x03).
        let mut ci = Vec::new();
        write_str(&mut ci, "en_us");
        ci.push(2); // view distance
        write_varint(&mut ci, 0); // chat mode
        ci.push(1); // chat colors
        ci.push(0x7f); // skin parts
        write_varint(&mut ci, 1); // main hand
        ci.push(0); // text filtering
        ci.push(1); // server listings
        send_packet(&mut cli, 0x00, ci).unwrap();

        loop {
            let (pid, _) = read_server_packet(&mut cli, deadline).expect("config packets");
            if pid == 0x0e {
                let mut kp = Vec::new();
                write_varint(&mut kp, 0);
                send_packet(&mut cli, 0x07, kp).unwrap();
            } else if pid == 0x03 {
                send_packet(&mut cli, 0x03, vec![]).unwrap();
                break;
            }
        }

        // Play: wait until Synchronize Player Position (0x40), then wait briefly until handle() pushes Client.
        loop {
            let (pid, body) = read_server_packet(&mut cli, deadline).expect("play initial position");
            if pid == 0x40 {
                send_packet(&mut cli, 0x00, body[33..].to_vec()).unwrap();
                break;
            }
        }

        let reg_deadline = Instant::now() + Duration::from_secs(2);
        let me = loop {
            if let Some(found) = clients()
                .lock()
                .unwrap()
                .iter()
                .find(|c| c.name == name)
                .cloned()
            {
                break found;
            }
            assert!(Instant::now() < reg_deadline, "registered play client");
            std::thread::sleep(Duration::from_millis(5));
        };
        (cli, server_handle, me)
    }

    fn decode_window_items(body: &[u8], expected_len: usize) -> Vec<Stack> {
        let mut sl = body;
        let _wid = sl[0];
        sl = &sl[1..];
        let _state = read_varint(&mut sl).unwrap();
        let count = read_varint(&mut sl).unwrap() as usize;
        assert_eq!(count, expected_len);
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let cnt = read_varint(&mut sl).unwrap();
            if cnt <= 0 {
                out.push((0, 0, Vec::new()));
                continue;
            }
            let item = read_varint(&mut sl).unwrap();
            let nadd = read_varint(&mut sl).unwrap();
            let nrem = read_varint(&mut sl).unwrap();
            assert_eq!(nrem, 0);
            let mut comps = Vec::new();
            for _ in 0..nadd {
                let ty = read_varint(&mut sl).unwrap();
                assert!(matches!(ty, 9 | 23), "unexpected test component {ty}");
                let start = sl;
                let pairs = read_varint(&mut sl).unwrap();
                for _ in 0..pairs {
                    let _id = read_varint(&mut sl).unwrap();
                    let _lvl = read_varint(&mut sl).unwrap();
                }
                let show_tip_consumed = 1usize;
                let data_len = start.len() - sl.len() + show_tip_consumed;
                sl = &sl[1..];
                comps.push((ty, start[..data_len].to_vec()));
            }
            out.push((item, cnt as u8, comps));
        }
        assert!(sl.len() >= 1); // carried item follows
        out
    }

    fn wait_packet(rx: &mut TcpStream, target_id: i64) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (pid, body) = read_server_packet(rx, deadline).expect("expected server packet");
            if pid == target_id {
                return body;
            }
        }
    }

    #[test]
    fn cart_deferred_loot_window_click_persistence_and_break_over_packets() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = RuntimeSnapshot::take();
        *WORLD_SPAWN.lock().unwrap() = (0.5, 70.0, 1.5, 0.0);
        forceloaded().lock().unwrap().insert((0, 0));
        refresh_hot();

        // Stage a seeded ChestMinecart (seed 1 has enchanted_book fire_aspect 1 at slot 7).
        materialize_generation_payloads(vec![(
            (0, 70, 0),
            crate::generation_payload::GenerationPayload::ChestMinecart { loot_seed: 1 },
        )]);
        let (cart_eid, cart_uuid) = {
            let gen = generated_content().lock().unwrap();
            assert_eq!(gen.carts.len(), 1);
            assert!(gen.carts[0].has_loot_table());
            assert!(gen.carts[0].data.get("Items").is_none());
            (gen.carts[0].eid, gen.carts[0].uuid().unwrap())
        };
        assert!(!chests().lock().unwrap().contains_key(&ContainerKey::Cart(cart_uuid)));

        // Save before any interaction: cart keeps LootTable and no Items.
        let mut pre_open_save = Vec::new();
        save_world_into(&mut pre_open_save);
        let (p, _) = read_world_seed_footer(&pre_open_save).unwrap();
        let (p, _) = read_mob_dimensions_footer(p).unwrap();
        let (p, _) = read_brewing_footer(p).unwrap();
        let (p, _) = read_dim2_footer(p).unwrap();
        let (p, _, _) = read_dim_footer(p).unwrap();
        let (p, _) = read_block_loot_footer(p).unwrap();
        let (p, _) = read_drop_owners_footer(p).unwrap();
        let pre_snap = generated_content::Snapshot::read(
            tick_persistence::prefix(p).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(pre_snap.content.carts[0].has_loot_table());
        assert!(pre_snap.content.carts[0].data.get("Items").is_none());

        // Connect real client via handle() and set survival mode.
        let (mut sock, server_thread, me) = connect_play_client("CartTester", [9u8; 16]);
        *me.gm.lock().unwrap() = 0;

        // Open the cart via 0x16 interact (mouse=0).
        let mut interact = Vec::new();
        write_varint(&mut interact, cart_eid as i64);
        write_varint(&mut interact, 0);
        write_varint(&mut interact, 0);
        interact.push(0);
        send_packet(&mut sock, 0x16, interact).unwrap();

        let open_pkt = wait_packet(&mut sock, 0x33);
        let mut op = &open_pkt[..];
        assert_eq!(read_varint(&mut op), Some(1));
        assert_eq!(read_varint(&mut op), Some(2));
        let items_pkt = wait_packet(&mut sock, 0x13);
        let slots = decode_window_items(&items_pkt, 27 + 36);

        let expected_entries = crate::chest_loot::abandoned_mineshaft(1);
        let book_entry = expected_entries
            .iter()
            .find(|e| e.item == "minecraft:enchanted_book")
            .expect("seed 1 produces enchanted book");
        for entry in &expected_entries {
            let st = &slots[entry.slot as usize];
            assert_eq!(st.0, name_to_item(entry.item).unwrap());
            assert_eq!(st.1, entry.count);
            if let Some((ename, lvl)) = entry.enchantment {
                assert_eq!(
                    comp_get(&st.2, COMP_STORED_ENCHANTMENTS),
                    Some(enchantments_data(&[(enchant_id(ename).unwrap(), i64::from(lvl))], true).as_slice())
                );
            }
        }
        {
            let gen = generated_content().lock().unwrap();
            assert!(!gen.carts[0].has_loot_table());
            assert!(gen.carts[0].data.get("Items").is_none());
        }

        // Shift-click (mode 1) the enchanted book out of the cart into player inventory.
        let mut click = vec![1u8];
        write_varint(&mut click, 0);
        click.extend_from_slice(&book_entry.slot.to_be_bytes());
        click.push(0);
        write_varint(&mut click, 1);
        write_varint(&mut click, 0);
        click.extend_from_slice(&slot_wire(&(0, 0, Vec::new())));
        send_packet(&mut sock, 0x0e, click).unwrap();

        let slot_update = wait_packet(&mut sock, 0x15);
        assert_eq!(slot_read_wire(&slot_update[2..]), Some((book_entry.slot, (0, 0, Vec::new()))));
        assert!(me.inv.lock().unwrap().values().any(|st| {
            st.0 == name_to_item("enchanted_book").unwrap()
                && comp_get(&st.2, COMP_STORED_ENCHANTMENTS).is_some()
        }));

        // Close window via 0x0f.
        send_packet(&mut sock, 0x0f, vec![1u8]).unwrap();
        let wait_close = Instant::now() + Duration::from_secs(2);
        while me.open_chest.lock().unwrap().is_some() {
            assert!(Instant::now() < wait_close, "open_chest did not clear on 0x0f");
            std::thread::sleep(Duration::from_millis(5));
        }

        // Save, clear, reload: mutated cart inventory persists without re-rolling.
        let mut mid_save = Vec::new();
        save_world_into(&mut mid_save);
        let expected_after_take = chests()
            .lock()
            .unwrap()
            .get(&ContainerKey::Cart(cart_uuid))
            .cloned()
            .unwrap();
        assert!(!expected_after_take.contains_key(&book_entry.slot));

        chests().lock().unwrap().clear();
        generated_content().lock().unwrap().carts.clear();
        load_world_from(&mid_save).unwrap();
        let reloaded_eid = {
            let gen = generated_content().lock().unwrap();
            assert_eq!(gen.carts.len(), 1);
            assert!(!gen.carts[0].has_loot_table());
            assert!(gen.carts[0].data.get("Items").is_none());
            gen.carts[0].eid
        };
        assert_eq!(
            chests().lock().unwrap().get(&ContainerKey::Cart(cart_uuid)),
            Some(&expected_after_take)
        );

        // Reopen reloaded cart via 0x16 -> still has no slot 7 enchanted book.
        let mut reopen = Vec::new();
        write_varint(&mut reopen, reloaded_eid as i64);
        write_varint(&mut reopen, 0);
        write_varint(&mut reopen, 0);
        reopen.push(0);
        send_packet(&mut sock, 0x16, reopen).unwrap();
        let _ = wait_packet(&mut sock, 0x33);
        let reopened_items = decode_window_items(&wait_packet(&mut sock, 0x13), 27 + 36);
        assert_eq!(reopened_items[book_entry.slot as usize], (0, 0, Vec::new()));

        // Empty the cart completely -> save -> reload -> remains empty and does not re-roll.
        chests().lock().unwrap().get_mut(&ContainerKey::Cart(cart_uuid)).unwrap().clear();
        let mut empty_save = Vec::new();
        save_world_into(&mut empty_save);
        chests().lock().unwrap().clear();
        generated_content().lock().unwrap().carts.clear();
        load_world_from(&empty_save).unwrap();
        let empty_reloaded_eid = {
            let gen = generated_content().lock().unwrap();
            assert!(!gen.carts[0].has_loot_table());
            gen.carts[0].eid
        };
        assert!(chests()
            .lock()
            .unwrap()
            .get(&ContainerKey::Cart(cart_uuid))
            .is_some_and(HashMap::is_empty));

        // Put a diamond inside, open it, and break the cart in survival (0x16 mouse=1).
        let diamond = name_to_item("diamond").unwrap();
        chests()
            .lock()
            .unwrap()
            .get_mut(&ContainerKey::Cart(cart_uuid))
            .unwrap()
            .insert(0, (diamond, 2, Vec::new()));
        let mut open_again = Vec::new();
        write_varint(&mut open_again, empty_reloaded_eid as i64);
        write_varint(&mut open_again, 0);
        write_varint(&mut open_again, 0);
        open_again.push(0);
        send_packet(&mut sock, 0x16, open_again).unwrap();
        let _ = wait_packet(&mut sock, 0x33);
        let _ = wait_packet(&mut sock, 0x13);

        // Register cart as seen so 0x42 destroy reaches this client.
        me.seen.lock().unwrap().insert(empty_reloaded_eid);
        let mut attack = Vec::new();
        write_varint(&mut attack, empty_reloaded_eid as i64);
        write_varint(&mut attack, 1);
        attack.push(0);
        send_packet(&mut sock, 0x16, attack).unwrap();

        let close_pkt = wait_packet(&mut sock, 0x12);
        assert_eq!(close_pkt, vec![1u8]);
        let destroy_pkt = wait_packet(&mut sock, 0x42);
        let mut dp = &destroy_pkt[..];
        assert_eq!(read_varint(&mut dp), Some(1));
        assert_eq!(read_varint(&mut dp), Some(empty_reloaded_eid as i64));

        assert!(generated_content().lock().unwrap().carts.is_empty());
        assert!(!chests().lock().unwrap().contains_key(&ContainerKey::Cart(cart_uuid)));
        assert!(me.open_chest.lock().unwrap().is_none());
        let dropped: Vec<(i64, u8)> = drops().lock().unwrap().iter().map(|d| (d.item, d.cnt)).collect();
        assert!(dropped.contains(&(name_to_item("chest_minecart").unwrap(), 1)));
        assert!(dropped.contains(&(diamond, 2)));

        // Unopened seeded cart broken in survival spills loot + chest_minecart.
        drops().lock().unwrap().clear();
        materialize_generation_payloads(vec![(
            (0, 70, 0),
            crate::generation_payload::GenerationPayload::ChestMinecart { loot_seed: 3 },
        )]);
        let unopened_eid = generated_content().lock().unwrap().carts[0].eid;
        let mut attack_unopened = Vec::new();
        write_varint(&mut attack_unopened, unopened_eid as i64);
        write_varint(&mut attack_unopened, 1);
        attack_unopened.push(0);
        send_packet(&mut sock, 0x16, attack_unopened).unwrap();
        let wait_drop = Instant::now() + Duration::from_secs(2);
        while !generated_content().lock().unwrap().carts.is_empty() {
            assert!(Instant::now() < wait_drop);
            std::thread::sleep(Duration::from_millis(5));
        }
        let has_book = drops()
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.item == name_to_item("enchanted_book").unwrap()
                && comp_get(&d.comps, COMP_STORED_ENCHANTMENTS).is_some());
        assert!(has_book, "breaking an unopened seeded cart in survival must spill rolled loot");

        let _ = sock.shutdown(Shutdown::Both);
        drop(sock);
        let _ = server_thread.join();
    }

    /// What one player got away with, per exploit, under one anti-cheat preset.
    #[derive(Debug)]
    struct AcOutcome {
        first_place_consumed: bool,
        second_place_with_empty_hand: bool,
        placed_into_stone: bool,
        air_placed: bool,
        teleport_accepted: bool,
        nofall_hp: f32,
    }

    fn ac_place(sock: &mut TcpStream, pos: BlockPos, face: i64, seq: i64) {
        let mut pkt = Vec::new();
        write_varint(&mut pkt, 0);
        write_pos(&mut pkt, pos.0, pos.1, pos.2);
        write_varint(&mut pkt, face);
        pkt.extend_from_slice(&0.5f32.to_be_bytes());
        pkt.extend_from_slice(&1.0f32.to_be_bytes());
        pkt.extend_from_slice(&0.5f32.to_be_bytes());
        pkt.push(0);
        write_varint(&mut pkt, seq);
        send_packet(sock, 0x38, pkt).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (pid, body) = read_server_packet(sock, deadline).expect("block ack");
            if pid == 0x05 && read_varint(&mut &body[..]) == Some(seq) {
                return;
            }
        }
    }

    fn ac_move(sock: &mut TcpStream, (x, y, z): (f64, f64, f64), ground: bool) {
        let mut b = Vec::new();
        b.extend_from_slice(&x.to_be_bytes());
        b.extend_from_slice(&y.to_be_bytes());
        b.extend_from_slice(&z.to_be_bytes());
        b.push(ground as u8);
        send_packet(sock, 0x1a, b).unwrap();
        std::thread::sleep(Duration::from_millis(60));
    }

    /// Confirm a pending server teleport (a setback), if one arrives.
    fn ac_confirm_teleport(sock: &mut TcpStream) -> bool {
        let deadline = Instant::now() + Duration::from_millis(800);
        while let Some((pid, body)) = read_server_packet(sock, deadline) {
            if pid == 0x40 && body.len() > 33 {
                let id = read_varint(&mut &body[33..]).unwrap();
                let mut c = Vec::new();
                write_varint(&mut c, id);
                send_packet(sock, 0x00, c).unwrap();
                std::thread::sleep(Duration::from_millis(60));
                return true;
            }
        }
        false
    }

    fn ac_floor() {
        let stone = name_to_state("stone").unwrap();
        for x in -2..=3 {
            for z in -2..=3 {
                set_block(x, 69, z, stone);
                set_block(x, 70, z, stone);
                for y in 71..=74 {
                    set_block(x, y, z, 0);
                }
            }
        }
    }

    fn ac_exploit_run(name: &str, uuid: [u8; 16], preset: AcPreset) -> AcOutcome {
        ac_test_set_player_preset(name, preset);
        ac_floor();
        let stone = name_to_state("stone").unwrap();
        let (mut sock, server_thread, me) = connect_play_client(name, uuid);
        *me.gm.lock().unwrap() = 0;
        {
            let mut inv = me.inv.lock().unwrap();
            inv.insert(36, (name_to_item("stone").unwrap(), 1, Vec::new()));
            inv.insert(37, (name_to_item("dirt").unwrap(), 1, Vec::new()));
        }
        *me.pos.lock().unwrap() = (0.5, 71.0, 0.5, 0.0, 0.0, true);
        // Infinite blocks: a one-item stack placed twice.
        ac_place(&mut sock, (1, 70, 0), 1, 1);
        assert_eq!(block_at(1, 71, 0), stone, "{name}: an honest placement must succeed");
        let first_place_consumed = me.inv.lock().unwrap().get(&36).is_none();
        ac_place(&mut sock, (1, 71, 0), 1, 2);
        let second_place_with_empty_hand = block_at(1, 72, 0) == stone;
        // Placing into a solid block, then against nothing.
        send_packet(&mut sock, 0x2f, 1i16.to_be_bytes().to_vec()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        ac_place(&mut sock, (2, 69, 0), 1, 3);
        let placed_into_stone = block_at(2, 70, 0) != stone;
        ac_place(&mut sock, (-1, 72, 0), 1, 4);
        let air_placed = block_at(-1, 73, 0) != 0;
        // A one-packet teleport.
        ac_move(&mut sock, (0.5, 71.0, 0.5), true);
        ac_move(&mut sock, (9000.5, 71.0, -9000.5), true);
        let teleport_accepted = me.pos.lock().unwrap().0 > 100.0;
        if teleport_accepted {
            ac_move(&mut sock, (0.5, 71.0, 0.5), true);
        } else {
            assert!(ac_confirm_teleport(&mut sock), "{name}: a setback teleport must be sent");
        }
        // NoFall: fall 29 blocks while claiming to stand on the ground.
        *me.hp.lock().unwrap() = 20.0;
        *me.pos.lock().unwrap() = (0.5, 100.0, 0.5, 0.0, 0.0, true);
        *me.fall_from.lock().unwrap() = 100.0;
        let mut y = 100.0;
        while y > 71.0 {
            y = (y - 3.0f64).max(71.0);
            ac_move(&mut sock, (0.5, y, 0.5), true);
        }
        std::thread::sleep(Duration::from_millis(100));
        let nofall_hp = *me.hp.lock().unwrap();
        let _ = sock.shutdown(Shutdown::Both);
        drop(sock);
        let _ = server_thread.join();
        AcOutcome {
            first_place_consumed,
            second_place_with_empty_hand,
            placed_into_stone,
            air_placed,
            teleport_accepted,
            nofall_hp,
        }
    }

    #[test]
    fn anticheat_live_standard_blocks_exploits_and_off_allows_them() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = RuntimeSnapshot::take();
        *WORLD_SPAWN.lock().unwrap() = (0.5, 71.0, 0.5, 0.0);
        forceloaded().lock().unwrap().insert((0, 0));
        refresh_hot();
        let off = ac_exploit_run("AcOff", [0x41u8; 16], AcPreset::Off);
        println!("ACLIVE off: {off:?}");
        let standard = ac_exploit_run("AcStandard", [0x42u8; 16], AcPreset::Standard);
        println!("ACLIVE standard: {standard:?}");
        assert!(!off.first_place_consumed && off.second_place_with_empty_hand, "{off:?}");
        assert!(off.placed_into_stone && off.air_placed && off.teleport_accepted, "{off:?}");
        assert_eq!(off.nofall_hp, 20.0, "{off:?}");
        assert!(standard.first_place_consumed && !standard.second_place_with_empty_hand, "{standard:?}");
        assert!(!standard.placed_into_stone && !standard.air_placed, "{standard:?}");
        assert!(!standard.teleport_accepted, "{standard:?}");
        assert!(standard.nofall_hp < 20.0, "{standard:?}");
    }

    /// Moves 2 blocks per packet (~33 blocks/s): fine with movement_speed 0.4 and Speed II,
    /// pulled back without them.
    fn ac_speed_run(name: &str, uuid: [u8; 16], boosted: bool) -> (u64, f64) {
        ac_test_set_player_preset(name, AcPreset::Standard);
        ac_floor();
        let (mut sock, server_thread, me) = connect_play_client(name, uuid);
        *me.gm.lock().unwrap() = 0;
        if boosted {
            attrs()
                .lock()
                .unwrap()
                .entry(format!("p:{name}"))
                .or_default()
                .insert("generic.movement_speed".into(), 0.4);
            me.effs.lock().unwrap().push(Eff { id: 0, amp: 1, left: 1_000_000 });
        }
        *me.pos.lock().unwrap() = (0.5, 71.0, 0.5, 0.0, 0.0, true);
        for i in 0..80 {
            let x = if i % 2 == 0 { 2.5 } else { 0.5 };
            ac_move(&mut sock, (x, 71.0, 0.5), true);
        }
        let flags = ac_flag_count(me.eid, AcCheckId::Movement);
        let vl = ac_violation_level(me.eid, AcCheckId::Movement);
        attrs().lock().unwrap().remove(&format!("p:{name}"));
        let _ = sock.shutdown(Shutdown::Both);
        drop(sock);
        let _ = server_thread.join();
        (flags, vl)
    }

    #[test]
    fn anticheat_live_speed_attribute_and_potion_are_not_flagged() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = RuntimeSnapshot::take();
        *WORLD_SPAWN.lock().unwrap() = (0.5, 71.0, 0.5, 0.0);
        forceloaded().lock().unwrap().insert((0, 0));
        refresh_hot();
        let boosted = ac_speed_run("AcBoosted", [0x43u8; 16], true);
        let plain = ac_speed_run("AcPlain", [0x44u8; 16], false);
        println!("ACLIVE speed: boosted (flags, vl) = {boosted:?}, plain = {plain:?}");
        assert_eq!(boosted.0, 0, "attribute + potion speed must not be flagged");
        assert!(plain.0 > 0, "the same speed without them must be");
    }

    #[test]
    fn anticheat_live_creative_spectator_and_ops_are_exempt_with_survival_grace() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = RuntimeSnapshot::take();
        *WORLD_SPAWN.lock().unwrap() = (0.5, 71.0, 0.5, 0.0);
        forceloaded().lock().unwrap().insert((0, 0));
        refresh_hot();
        ac_floor();
        let x_of = |me: &Arc<Client>| me.pos.lock().unwrap().0;

        ac_test_set_player_preset("AcBuilder", AcPreset::Standard);
        let (mut sock, server_thread, me) = connect_play_client("AcBuilder", [0x45u8; 16]);
        *me.gm.lock().unwrap() = 1;
        *me.pos.lock().unwrap() = (0.5, 71.0, 0.5, 0.0, 0.0, true);
        me.inv.lock().unwrap().insert(36, (name_to_item("dirt").unwrap(), 1, Vec::new()));
        // Creative: a 40-block flight in one packet and an air-placement are fine.
        ac_move(&mut sock, (40.5, 90.0, 0.5), false);
        assert_eq!(x_of(&me), 40.5, "creative flight must not be pulled back");
        assert_eq!(ac_exemption(&me), Some("creative"));
        ac_move(&mut sock, (0.5, 71.0, 0.5), true);
        ac_place(&mut sock, (-1, 72, 0), 1, 1);
        assert_ne!(block_at(-1, 73, 0), 0, "creative may place against air");
        // Spectator: same.
        *me.gm.lock().unwrap() = 3;
        ac_move(&mut sock, (30.5, 60.0, 0.5), false);
        assert_eq!(x_of(&me), 30.5, "spectator flight must not be pulled back");
        // Back to survival mid-air: the landing is graced...
        *me.gm.lock().unwrap() = 0;
        ac_move(&mut sock, (20.5, 71.0, 0.5), true);
        assert_eq!(ac_exemption(&me), Some("game mode change"));
        assert_eq!(x_of(&me), 20.5, "the move right after leaving spectator is graced");
        // ...and once the grace runs out survival is checked again.
        std::thread::sleep(Duration::from_millis(3200));
        assert_eq!(ac_exemption(&me), None);
        ac_move(&mut sock, (9000.5, 71.0, 0.5), true);
        assert_eq!(x_of(&me), 20.5, "survival teleport must be pulled back after the grace");
        assert!(ac_confirm_teleport(&mut sock));
        assert_eq!(ac_flag_count(me.eid, AcCheckId::Movement), 1);
        let _ = sock.shutdown(Shutdown::Both);
        drop(sock);
        let _ = server_thread.join();

        // An operator in survival is trusted (check_ops defaults to false).
        ac_test_set_player_preset("AcOperator", AcPreset::Standard);
        std::env::set_var("MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS", "1");
        let (mut sock, server_thread, me) = connect_play_client("AcOperator", [0x46u8; 16]);
        *me.gm.lock().unwrap() = 0;
        *me.pos.lock().unwrap() = (0.5, 71.0, 0.5, 0.0, 0.0, true);
        ac_move(&mut sock, (9000.5, 71.0, 0.5), true);
        let op_x = x_of(&me);
        let reason = ac_exemption(&me);
        std::env::remove_var("MC_SPINE_ALLOW_PLAYER_ADMIN_COMMANDS");
        assert_eq!(reason, Some("operator"));
        assert_eq!(op_x, 9000.5, "an operator's move is not checked");
        ac_move(&mut sock, (0.5, 71.0, 0.5), true);
        let _ = sock.shutdown(Shutdown::Both);
        drop(sock);
        let _ = server_thread.join();
    }

    #[test]
    fn chest_contract_live_tcp_blocked_broadcast_and_reach() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = RuntimeSnapshot::take();
        *WORLD_SPAWN.lock().unwrap() = (0.5, 70.0, 0.5, 0.0);
        forceloaded().lock().unwrap().insert((0, 0));
        refresh_hot();

        let chest_st = name_to_state("chest").unwrap();
        let stone_st = name_to_state("stone").unwrap();
        let ender_st = name_to_state("ender_chest").unwrap();
        let barrel_st = name_to_state("barrel").unwrap();

        let blocked_single: BlockPos = (2, 70, 0);
        set_block(blocked_single.0, blocked_single.1, blocked_single.2, chest_st);
        set_block(blocked_single.0, blocked_single.1 + 1, blocked_single.2, stone_st);

        let blocked_left: BlockPos = (2, 70, 3);
        let blocked_right: BlockPos = (3, 70, 3);
        set_block(blocked_left.0, blocked_left.1, blocked_left.2, chest_st);
        set_block(
            blocked_right.0,
            blocked_right.1,
            blocked_right.2,
            chest_placement_state("minecraft:chest", blocked_right, 0.0, false, 1),
        );
        set_block(blocked_left.0, blocked_left.1 + 1, blocked_left.2, stone_st);

        let blocked_ender: BlockPos = (2, 70, 6);
        set_block(blocked_ender.0, blocked_ender.1, blocked_ender.2, ender_st);
        set_block(blocked_ender.0, blocked_ender.1 + 1, blocked_ender.2, stone_st);

        let barrel: BlockPos = (2, 70, 9);
        set_block(barrel.0, barrel.1, barrel.2, barrel_st);
        set_block(barrel.0, barrel.1 + 1, barrel.2, stone_st);

        let open_left: BlockPos = (4, 70, 0);
        let open_right: BlockPos = (5, 70, 0);
        set_block(open_left.0, open_left.1, open_left.2, chest_st);
        set_block(
            open_right.0,
            open_right.1,
            open_right.2,
            chest_placement_state("minecraft:chest", open_right, 0.0, false, 1),
        );
        assert_eq!(
            chest_window(open_left.0, open_left.1, open_left.2),
            (
                ContainerKey::ow(open_right),
                Some(ContainerKey::ow(open_left))
            )
        );

        materialize_generation_payloads(vec![(
            (0, 70, 2),
            crate::generation_payload::GenerationPayload::ChestMinecart { loot_seed: 2 },
        )]);
        set_block(0, 71, 2, stone_st);
        let cart_eid = generated_content().lock().unwrap().carts[0].eid;

        let (mut sock_a, th_a, alice) = connect_play_client("Alice", [0x11u8; 16]);
        let (mut sock_b, th_b, bob) = connect_play_client("Bob", [0x22u8; 16]);
        *alice.gm.lock().unwrap() = 0;
        *bob.gm.lock().unwrap() = 0;

        let use_until_ack = |sock: &mut TcpStream, pos: BlockPos, seq: i64| -> Vec<(i64, Vec<u8>)> {
            let mut pkt = Vec::new();
            write_varint(&mut pkt, 0); // main hand
            write_pos(&mut pkt, pos.0, pos.1, pos.2);
            write_varint(&mut pkt, 1); // UP face
            pkt.extend_from_slice(&0.5f32.to_be_bytes());
            pkt.extend_from_slice(&1.0f32.to_be_bytes());
            pkt.extend_from_slice(&0.5f32.to_be_bytes());
            pkt.push(0);
            write_varint(&mut pkt, seq);
            send_packet(sock, 0x38, pkt).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut seen = Vec::new();
            loop {
                let (pid, body) = read_server_packet(sock, deadline).expect("expected 0x05 ack");
                if pid == 0x05 {
                    let mut sl = &body[..];
                    if read_varint(&mut sl) == Some(seq) {
                        return seen;
                    }
                }
                seen.push((pid, body));
            }
        };

        // 1. Blocked single, blocked double (via left or right), and blocked ender chest refuse open.
        *alice.pos.lock().unwrap() = (2.5, 70.0, 1.5, 0.0, 0.0, true);
        for (idx, pos) in [blocked_single, blocked_left, blocked_right, blocked_ender]
            .into_iter()
            .enumerate()
        {
            if pos == blocked_ender {
                *alice.pos.lock().unwrap() = (2.5, 70.0, 5.5, 0.0, 0.0, true);
            }
            let pkts = use_until_ack(&mut sock_a, pos, 100 + idx as i64);
            assert!(
                !pkts.iter().any(|(pid, _)| matches!(*pid, 0x33 | 0x13 | 0x08)),
                "blocked container at {pos:?} must not open or emit blockEvent"
            );
            assert!(alice.open_chest.lock().unwrap().is_none());
        }

        // 2. Barrel and minecart chest under stone still open normally.
        *alice.pos.lock().unwrap() = (2.5, 70.0, 8.5, 0.0, 0.0, true);
        let barrel_pkts = use_until_ack(&mut sock_a, barrel, 200);
        assert!(
            barrel_pkts.iter().any(|(pid, _)| *pid == 0x33),
            "barrel under stone must still open"
        );
        assert!(alice.open_chest.lock().unwrap().is_some());
        send_packet(&mut sock_a, 0x0f, vec![1u8]).unwrap();
        let wait_close = Instant::now() + Duration::from_secs(2);
        while alice.open_chest.lock().unwrap().is_some() {
            assert!(Instant::now() < wait_close);
            std::thread::sleep(Duration::from_millis(5));
        }

        *alice.pos.lock().unwrap() = (0.5, 70.0, 2.5, 0.0, 0.0, true);
        let mut cart_open = Vec::new();
        write_varint(&mut cart_open, cart_eid as i64);
        write_varint(&mut cart_open, 0);
        write_varint(&mut cart_open, 0);
        cart_open.push(0);
        send_packet(&mut sock_a, 0x16, cart_open).unwrap();
        let _ = wait_packet(&mut sock_a, 0x33);
        let _ = wait_packet(&mut sock_a, 0x13);
        *alice.pos.lock().unwrap() = (0.5, 70.0, 10.5, 0.0, 0.0, true);
        game_tick(|_, tick| tick());
        assert_eq!(wait_packet(&mut sock_a, 0x12), vec![1u8]);
        assert!(alice.open_chest.lock().unwrap().is_none());

        // 3. Alice & Bob open the unblocked double chest -> Alice clicks -> Bob gets 0x15 on tick.
        *alice.pos.lock().unwrap() = (4.5, 70.0, 1.5, 0.0, 0.0, true);
        *bob.pos.lock().unwrap() = (4.5, 70.0, 1.5, 0.0, 0.0, true);
        let open_a = use_until_ack(&mut sock_a, open_left, 301);
        assert!(open_a.iter().any(|(pid, _)| *pid == 0x33));
        let open_b = use_until_ack(&mut sock_b, open_right, 302);
        assert!(open_b.iter().any(|(pid, _)| *pid == 0x33));

        let diamond = name_to_item("diamond").unwrap();
        alice.inv.lock().unwrap().insert(9, (diamond, 1, Vec::new()));
        let mut click1 = vec![1u8];
        write_varint(&mut click1, 0);
        click1.extend_from_slice(&54i16.to_be_bytes()); // inv slot 9 in 54-slot chest
        click1.push(0);
        write_varint(&mut click1, 0);
        write_varint(&mut click1, 0);
        click1.extend_from_slice(&slot_wire(&(0, 0, Vec::new())));
        send_packet(&mut sock_a, 0x0e, click1).unwrap();
        let _ = wait_packet(&mut sock_a, 0x15);

        let mut click2 = vec![1u8];
        write_varint(&mut click2, 0);
        click2.extend_from_slice(&30i16.to_be_bytes()); // left half slot 3 (27 + 3)
        click2.push(0);
        write_varint(&mut click2, 0);
        write_varint(&mut click2, 0);
        click2.extend_from_slice(&slot_wire(&(0, 0, Vec::new())));
        send_packet(&mut sock_a, 0x0e, click2).unwrap();
        let alice_echo = wait_packet(&mut sock_a, 0x15);
        assert_eq!(
            slot_read_wire(&alice_echo[2..]),
            Some((30, (diamond, 1, Vec::new())))
        );

        game_tick(|_, tick| tick());
        let bob_sync = wait_packet(&mut sock_b, 0x15);
        let mut sl = &bob_sync[1..];
        let st_id = read_varint(&mut sl).unwrap();
        assert!(st_id >= 1);
        assert_eq!(slot_read_wire(sl), Some((30, (diamond, 1, Vec::new()))));

        // 4. Alice walks out of reach -> game_tick closes her window (0x12) and drops 0x08 to 1.
        *alice.pos.lock().unwrap() = (15.0, 70.0, 0.5, 0.0, 0.0, true);
        game_tick(|_, tick| tick());
        assert_eq!(wait_packet(&mut sock_a, 0x12), vec![1u8]);
        assert!(alice.open_chest.lock().unwrap().is_none());

        let mut dropped_counts = Vec::new();
        for _ in 0..2 {
            let ev = wait_packet(&mut sock_b, 0x08);
            let pos = read_pos(&ev[..8]);
            dropped_counts.push((pos, ev[8], ev[9]));
        }
        assert!(dropped_counts.contains(&(open_right, 1, 1)));
        assert!(dropped_counts.contains(&(open_left, 1, 1)));

        let _ = sock_a.shutdown(Shutdown::Both);
        let _ = sock_b.shutdown(Shutdown::Both);
        drop(sock_a);
        drop(sock_b);
        let _ = th_a.join();
        let _ = th_b.join();
    }

    #[test]
    fn chest_contract_3d_live_tcp_lazy_loot_barrel_ender_and_waterlogged() {
        if crate::test_support::isolated_world_test() {
            return;
        }
        crate::test_support::require_vanilla_data();
        let _guard = world_guard();
        let _snap = RuntimeSnapshot::take();
        *WORLD_SPAWN.lock().unwrap() = (0.5, 70.0, 0.5, 0.0);
        forceloaded().lock().unwrap().insert((0, 0));
        refresh_hot();

        let reg = crate::block_registry::breg().expect("block registry");
        let chest_st = name_to_state("chest").unwrap();
        let barrel_closed = reg.with_prop(
            reg.with_prop(reg.state_of("minecraft:barrel", None), "facing", "east"),
            "open",
            "false",
        );
        let barrel_open = reg.with_prop(barrel_closed, "open", "true");
        let ender_st = name_to_state("ender_chest").unwrap();
        let ender_id = i64::from(reg.block_id_of(ender_st).unwrap());

        let loot_chest: BlockPos = (2, 70, 0);
        let barrel_pos: BlockPos = (2, 70, 3);
        let ender_pos: BlockPos = (2, 70, 6);
        set_block(loot_chest.0, loot_chest.1, loot_chest.2, chest_st);
        set_block(barrel_pos.0, barrel_pos.1, barrel_pos.2, barrel_closed);
        set_block(ender_pos.0, ender_pos.1, ender_pos.2, ender_st);

        materialize_generation_payloads(vec![
            (
                loot_chest,
                crate::generation_payload::GenerationPayload::LootContainer {
                    kind: crate::generation_payload::ContainerKind::Chest,
                    loot_table: "minecraft:chests/abandoned_mineshaft".to_owned(),
                    loot_seed: Some(77777),
                },
            ),
            (
                (0, 70, 2),
                crate::generation_payload::GenerationPayload::ChestMinecart { loot_seed: 9 },
            ),
        ]);
        let cart_eid = generated_content().lock().unwrap().carts[0].eid;

        let (mut sock_a, th_a, alice) = connect_play_client("Alice", [0x11u8; 16]);
        let (mut sock_b, th_b, bob) = connect_play_client("Bob", [0x22u8; 16]);

        let use_until_ack = |sock: &mut TcpStream, pos: BlockPos, seq: i64| -> Vec<(i64, Vec<u8>)> {
            let mut pkt = Vec::new();
            write_varint(&mut pkt, 0);
            write_pos(&mut pkt, pos.0, pos.1, pos.2);
            write_varint(&mut pkt, 1);
            pkt.extend_from_slice(&0.5f32.to_be_bytes());
            pkt.extend_from_slice(&1.0f32.to_be_bytes());
            pkt.extend_from_slice(&0.5f32.to_be_bytes());
            pkt.push(0);
            write_varint(&mut pkt, seq);
            send_packet(sock, 0x38, pkt).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut seen = Vec::new();
            loop {
                let (pid, body) = read_server_packet(sock, deadline).expect("expected 0x05 ack");
                if pid == 0x05 {
                    let mut sl = &body[..];
                    if read_varint(&mut sl) == Some(seq) {
                        return seen;
                    }
                }
                seen.push((pid, body));
            }
        };

        // 1. Spectator cannot open or trigger lazy LootTable materialization; survival unpacks once.
        *alice.gm.lock().unwrap() = 3;
        *alice.pos.lock().unwrap() = (2.5, 70.0, 1.5, 0.0, 0.0, true);
        let spec_pkts = use_until_ack(&mut sock_a, loot_chest, 1);
        assert!(
            !spec_pkts.iter().any(|(pid, _)| matches!(*pid, 0x33 | 0x13)),
            "spectator must not open unopened loot chest"
        );
        assert!(pending_block_loot().lock().unwrap().contains_key(&ow_block(loot_chest)));
        assert!(!chests().lock().unwrap().contains_key(&ContainerKey::ow(loot_chest)));

        *alice.gm.lock().unwrap() = 0;
        let surv_pkts = use_until_ack(&mut sock_a, loot_chest, 2);
        assert!(surv_pkts.iter().any(|(pid, _)| *pid == 0x33));
        assert!(!pending_block_loot().lock().unwrap().contains_key(&ow_block(loot_chest)));
        assert_eq!(
            chests().lock().unwrap().get(&ContainerKey::ow(loot_chest)).cloned(),
            Some(roll_cart_loot(77777))
        );
        send_packet(&mut sock_a, 0x0f, vec![1u8]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while alice.open_chest.lock().unwrap().is_some() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }

        // 2. Barrel 0x38 open/close toggles open=true/false via 0x09 on Bob, never 0x08.
        *alice.pos.lock().unwrap() = (2.5, 70.0, 4.5, 0.0, 0.0, true);
        *bob.pos.lock().unwrap() = (2.5, 70.0, 4.5, 0.0, 0.0, true);
        let _ = use_until_ack(&mut sock_a, barrel_pos, 3);
        let mut bob_open_upd = Vec::new();
        for _ in 0..8 {
            let body = wait_packet(&mut sock_b, 0x09);
            let mut sl = &body[8..];
            let st = read_varint(&mut sl).unwrap() as u32;
            bob_open_upd.push((read_pos(&body[..8]), st));
            if bob_open_upd.contains(&(barrel_pos, barrel_open)) {
                break;
            }
        }
        assert!(bob_open_upd.contains(&(barrel_pos, barrel_open)));
        send_packet(&mut sock_a, 0x0f, vec![1u8]).unwrap();
        let mut bob_close_upd = Vec::new();
        for _ in 0..8 {
            let body = wait_packet(&mut sock_b, 0x09);
            let mut sl = &body[8..];
            let st = read_varint(&mut sl).unwrap() as u32;
            bob_close_upd.push((read_pos(&body[..8]), st));
            if bob_close_upd.contains(&(barrel_pos, barrel_closed)) {
                break;
            }
        }
        assert!(bob_close_upd.contains(&(barrel_pos, barrel_closed)));

        // 3. Ender chest 0x38 open emits 0x08 (action=1, count=1), and break closes with 0x12.
        let emerald = name_to_item("emerald").unwrap();
        chests()
            .lock()
            .unwrap()
            .entry(ender_container_key(&alice.uuid))
            .or_default()
            .insert(0, (emerald, 2, Vec::new()));
        *alice.pos.lock().unwrap() = (2.5, 70.0, 7.5, 0.0, 0.0, true);
        let _ = use_until_ack(&mut sock_a, ender_pos, 4);
        let ev = wait_packet(&mut sock_b, 0x08);
        assert_eq!(read_pos(&ev[..8]), ender_pos);
        assert_eq!((ev[8], ev[9]), (1, 1));
        let mut ev_sl = &ev[10..];
        assert_eq!(read_varint(&mut ev_sl), Some(ender_id));

        set_block(ender_pos.0, ender_pos.1, ender_pos.2, 0);
        assert_eq!(wait_packet(&mut sock_a, 0x12), vec![1u8]);
        assert!(alice.open_chest.lock().unwrap().is_none());
        assert_eq!(
            chests()
                .lock()
                .unwrap()
                .get(&ender_container_key(&alice.uuid))
                .and_then(|m| m.get(&0))
                .map(|s| (s.0, s.1)),
            Some((emerald, 2))
        );

        // 4. MinecartChest 0x16 opens 0x33+0x13 without emitting any 0x08 lid event.
        *alice.pos.lock().unwrap() = (0.5, 70.0, 2.5, 0.0, 0.0, true);
        let mut cart_open = Vec::new();
        write_varint(&mut cart_open, cart_eid as i64);
        write_varint(&mut cart_open, 0);
        write_varint(&mut cart_open, 0);
        cart_open.push(0);
        send_packet(&mut sock_a, 0x16, cart_open).unwrap();
        let _ = wait_packet(&mut sock_a, 0x33);
        let _ = wait_packet(&mut sock_a, 0x13);

        let _ = sock_a.shutdown(Shutdown::Both);
        let _ = sock_b.shutdown(Shutdown::Both);
        drop(sock_a);
        drop(sock_b);
        let _ = th_a.join();
        let _ = th_b.join();
    }

    #[test]
    fn spawner_runtime_tick_light_cap_break_and_persistence() {
        let _guard = world_guard();
        let _snap = RuntimeSnapshot::take();
        DIFFICULTY.store(2, Ordering::Relaxed);
        WORLD_TIME.store(6000, Ordering::Relaxed);
        forceloaded().lock().unwrap().insert((0, 0));
        refresh_hot();

        // Carve an enclosed underground stone chamber well below surface terrain height (y >= 8)
        // so sky_light is 0 inside while block_light stays 0 until a torch is placed.
        let stone = name_to_state("stone").unwrap();
        for x in -6i32..=6 {
            for z in -6i32..=6 {
                for y in -22..=-17 {
                    let wall = x.abs() == 6 || z.abs() == 6 || y == -22 || y == -17;
                    set_block(x, y, z, if wall { stone } else { 0 });
                }
            }
        }
        let spawner_state = name_to_state("spawner").unwrap();
        set_block(0, -20, 0, spawner_state);
        let mut sp_nbt = generated_content::spawner_nbt((0, -20, 0));
        if let crate::nbt::Tag::Compound(ref mut f) = sp_nbt {
            f.insert(
                crate::nbt::NbtString::text("Delay"),
                crate::nbt::Tag::Short(2),
            );
        }
        generated_content().lock().unwrap().spawners.insert((0, -20, 0), sp_nbt);

        let (client, mut receiver) = effect_tests::client_pair();
        *client.pos.lock().unwrap() = (6.5, -20.0, 0.5, 0.0, 0.0, true);
        *client.gm.lock().unwrap() = 0;
        let client = Arc::new(client);
        clients().lock().unwrap().push(client.clone());

        // Delay 2 -> 1 -> 0 -> fire on 3rd tick
        game_tick(|_, tick| tick());
        assert_eq!(
            generated_content().lock().unwrap().spawners[&(0, -20, 0)]
                .get("Delay")
                .and_then(|t| match t { crate::nbt::Tag::Short(v) => Some(*v), _ => None }),
            Some(1)
        );
        assert!(mobs().lock().unwrap().is_empty());

        game_tick(|_, tick| tick());
        assert_eq!(
            generated_content().lock().unwrap().spawners[&(0, -20, 0)]
                .get("Delay")
                .and_then(|t| match t { crate::nbt::Tag::Short(v) => Some(*v), _ => None }),
            Some(0)
        );

        let cave_spider = name_to_etype("cave_spider").unwrap();
        let mut spawned = false;
        for _ in 0..8 {
            if let crate::nbt::Tag::Compound(ref mut f) =
                generated_content().lock().unwrap().spawners.get_mut(&(0, -20, 0)).unwrap()
            {
                if !spawned {
                    f.insert(crate::nbt::NbtString::text("Delay"), crate::nbt::Tag::Short(0));
                }
            }
            game_tick(|_, tick| tick());
            if mobs().lock().unwrap().iter().any(|m| m.etype == cave_spider) {
                spawned = true;
                break;
            }
        }
        assert!(spawned, "spawner in dark enclosed chamber should insert cave_spider");
        let spider_hp = mobs()
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.etype == cave_spider)
            .map(|m| *m.hp.lock().unwrap())
            .unwrap();
        assert_eq!(spider_hp, 12.0);
        let reset_delay = generated_content().lock().unwrap().spawners[&(0, -20, 0)]
            .get("Delay")
            .and_then(|t| match t { crate::nbt::Tag::Short(v) => Some(*v), _ => None })
            .unwrap();
        assert!((200..=800).contains(&reset_delay), "unexpected reset Delay={reset_delay}");

        // Check world event 2004 packet sent to nearby player.
        let _ = client.sock.lock().unwrap().shutdown(Shutdown::Write);
        let mut got_2004 = false;
        while let Some((pid, body)) = read_packet(&mut receiver, Instant::now() + Duration::from_secs(1)) {
            if pid == 0x28 && body.len() >= 4 && i32::from_be_bytes(body[..4].try_into().unwrap()) == 2004 {
                got_2004 = true;
                break;
            }
        }
        assert!(got_2004, "spawner spawn should broadcast 0x28 world event 2004");

        // Save/load preserves mutated Delay and spawned cave spiders.
        let mut saved = Vec::new();
        save_world_into(&mut saved);
        let spider_count_before = mobs().lock().unwrap().iter().filter(|m| m.etype == cave_spider).count();
        mobs().lock().unwrap().clear();
        generated_content().lock().unwrap().spawners.clear();
        load_world_from(&saved).unwrap();
        assert_eq!(
            generated_content().lock().unwrap().spawners[&(0, -20, 0)]
                .get("Delay")
                .and_then(|t| match t { crate::nbt::Tag::Short(v) => Some(*v), _ => None }),
            Some(reset_delay)
        );
        assert_eq!(
            mobs().lock().unwrap().iter().filter(|m| m.etype == cave_spider).count(),
            spider_count_before
        );

        // Torch light blocks spawning and keeps Delay == 0.
        mobs().lock().unwrap().clear();
        let torch = name_to_state("torch").unwrap();
        set_block(0, -19, 0, torch);
        if let crate::nbt::Tag::Compound(ref mut f) =
            generated_content().lock().unwrap().spawners.get_mut(&(0, -20, 0)).unwrap()
        {
            f.insert(crate::nbt::NbtString::text("Delay"), crate::nbt::Tag::Short(0));
        }
        spawner_tick();
        assert!(mobs().lock().unwrap().is_empty(), "torch light must suppress cave spider spawn");
        assert_eq!(
            generated_content().lock().unwrap().spawners[&(0, -20, 0)]
                .get("Delay")
                .and_then(|t| match t { crate::nbt::Tag::Short(v) => Some(*v), _ => None }),
            Some(0)
        );
        set_block(0, -19, 0, 0);

        // MaxNearbyEntities=6 cap prevents spawning above 6 and resets Delay.
        for i in 0..6 {
            mobs().lock().unwrap().push(Arc::new(Mob {
                dim: DimId::Overworld,
                eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                uuid: [i as u8 + 10; 16],
                etype: cave_spider,
                pos: Mutex::new((0.5, -20.0, 0.5, 0.0, 0.0)),
                hp: Mutex::new(12.0),
                last_hit: Mutex::new(Instant::now()),
                hurt_at: Mutex::new(Instant::now()),
                last_hurt_tick: Mutex::new(None),
                vel: Mutex::new([0.0; 3]),
                fuse: Mutex::new(0),
                fire_ticks: Mutex::new(0),
                love: Mutex::new(0),
                air: Mutex::new(60),
            }));
        }
        if let crate::nbt::Tag::Compound(ref mut f) =
            generated_content().lock().unwrap().spawners.get_mut(&(0, -20, 0)).unwrap()
        {
            f.insert(crate::nbt::NbtString::text("Delay"), crate::nbt::Tag::Short(0));
        }
        spawner_tick();
        assert_eq!(mobs().lock().unwrap().len(), 6);
        let cap_delay = generated_content().lock().unwrap().spawners[&(0, -20, 0)]
            .get("Delay")
            .and_then(|t| match t { crate::nbt::Tag::Short(v) => Some(*v), _ => None })
            .unwrap();
        assert!((200..=800).contains(&cap_delay));

        // Breaking the spawner block removes its owned NBT entry.
        set_block(0, -20, 0, 0);
        assert!(!generated_content().lock().unwrap().spawners.contains_key(&(0, -20, 0)));
    }

    #[test]
    fn cart_runtime_tick_motion_collision_comparator_tracker_and_persistence() {
        let _guard = world_guard();
        let _snap = RuntimeSnapshot::take();
        forceloaded().lock().unwrap().insert((0, 0));
        refresh_hot();

        let reg = crate::block_registry::breg().unwrap();
        let stone = name_to_state("stone").unwrap();
        for x in -2..=10 {
            for z in -2..=2 {
                set_block(x, 69, z, stone);
            }
        }
        let powered_east = reg
            .checked_state(
                "minecraft:powered_rail",
                &[("powered".into(), "true".into()), ("shape".into(), "east_west".into())],
            )
            .unwrap();
        let rail_east = reg
            .checked_state("minecraft:rail", &[("shape".into(), "east_west".into())])
            .unwrap();
        set_block(-1, 70, 0, stone); // conductor behind powered rail
        set_block(0, 70, 0, powered_east);
        for x in 1..=6 {
            set_block(x, 70, 0, rail_east);
        }

        materialize_generation_payloads(vec![(
            (0, 70, 0),
            crate::generation_payload::GenerationPayload::ChestMinecart { loot_seed: 1 },
        )]);
        let cart_eid = {
            let mut gen = generated_content().lock().unwrap();
            gen.carts[0].set_pos([0.5, 70.0625, 0.5]);
            gen.carts[0].eid
        };

        let (client, mut receiver) = effect_tests::client_pair();
        *client.pos.lock().unwrap() = (2.0, 70.0, 0.5, 0.0, 0.0, true);
        client.chunks.lock().unwrap().insert((0, 0));
        let client = Arc::new(client);
        clients().lock().unwrap().push(client.clone());

        // Interest pass sends spawn packet; 2 game_ticks advance powered cart (tick 1 boosts vel=0.02, tick 2 moves east) and broadcast 0x70 + 0x5a.
        interest_tick();
        game_tick(|_, tick| tick());
        game_tick(|_, tick| tick());
        let (pos1, vel1) = {
            let gen = generated_content().lock().unwrap();
            (gen.carts[0].pos().unwrap(), gen.carts[0].motion())
        };
        assert!(pos1[0] > 0.5 && vel1[0] > 0.0, "powered rail with conductor must launch cart east: pos={pos1:?} vel={vel1:?}");

        let _ = client.sock.lock().unwrap().shutdown(Shutdown::Write);
        let mut saw_spawn = false;
        let mut saw_teleport = false;
        let mut saw_velocity = false;
        while let Some((pid, body)) = read_packet(&mut receiver, Instant::now() + Duration::from_secs(1)) {
            if pid == 0x01 && !body.is_empty() {
                saw_spawn = true;
            } else if pid == 0x70 && read_varint(&mut &body[..]) == Some(cart_eid as i64) {
                saw_teleport = true;
            } else if pid == 0x5a && read_varint(&mut &body[..]) == Some(cart_eid as i64) {
                saw_velocity = true;
            }
        }
        assert!(saw_spawn && saw_teleport && saw_velocity, "tracker packets missing: spawn={saw_spawn} tp={saw_teleport} vel={saw_velocity}");

        // Save/load preserves updated Pos and Motion, and spawn packet reflects motion.
        let mut saved = Vec::new();
        save_world_into(&mut saved);
        generated_content().lock().unwrap().carts.clear();
        load_world_from(&saved).unwrap();
        let reloaded = generated_content().lock().unwrap().carts[0].clone();
        assert_eq!(reloaded.pos().unwrap(), pos1);
        assert_eq!(reloaded.motion(), vel1);
        let spawn_bytes = spawn_generated_cart(&reloaded);
        assert_eq!(
            &spawn_bytes[spawn_bytes.len() - 6..spawn_bytes.len() - 4],
            &((vel1[0].clamp(-3.9, 3.9) * 8000.0) as i16).to_be_bytes()
        );

        // Comparator drag: empty unpacked cart (signal=0, drag=0.995) vs pending (drag=0.98) vs full unpacked (signal=15, drag=0.98).
        let uuid_a = [0x11u8; 16];
        let uuid_b = [0x22u8; 16];
        let uuid_c = [0x33u8; 16];
        {
            let stone_item = name_to_item("stone").unwrap();
            let mut full_slots = HashMap::new();
            for s in 0..27i16 {
                full_slots.insert(s, (stone_item, 64, Vec::new()));
            }
            chests()
                .lock()
                .unwrap()
                .insert(ContainerKey::Cart(uuid_c), full_slots);

            let mut gen = generated_content().lock().unwrap();
            gen.carts.clear();
            let mut pending = generated_content::Cart::new(1001, uuid_a, (2, 70, 0), 1);
            pending.set_pos([2.5, 70.0625, 0.5]);
            pending.set_motion([0.3, 0.0, 0.0]);
            let mut unpacked_empty = generated_content::Cart::new(1002, uuid_b, (4, 70, 0), 1);
            unpacked_empty.take_loot();
            unpacked_empty.set_pos([4.5, 70.0625, 0.5]);
            unpacked_empty.set_motion([0.3, 0.0, 0.0]);
            let mut unpacked_full = generated_content::Cart::new(1003, uuid_c, (3, 70, 0), 1);
            unpacked_full.take_loot();
            unpacked_full.set_pos([3.5, 70.0625, 0.5]);
            unpacked_full.set_motion([0.3, 0.0, 0.0]);
            gen.carts.push(pending);
            gen.carts.push(unpacked_empty);
            gen.carts.push(unpacked_full);
        }
        cart_tick();
        let (v_pending, v_empty, v_full) = {
            let gen = generated_content().lock().unwrap();
            (
                gen.carts[0].motion()[0],
                gen.carts[1].motion()[0],
                gen.carts[2].motion()[0],
            )
        };
        assert!(v_empty > v_pending, "empty unpacked cart should preserve more speed ({v_empty} vs {v_pending})");
        assert!(v_empty > v_full, "empty unpacked cart should outrun full unpacked cart ({v_empty} vs {v_full})");
        assert!((v_full - v_pending).abs() < 1e-9, "full unpacked cart (signal=15) matches pending drag ({v_full} vs {v_pending})");

        // Obstacle collision stops cart along track.
        set_block(5, 70, 0, stone);
        {
            let mut gen = generated_content().lock().unwrap();
            gen.carts.truncate(1);
            gen.carts[0].set_pos([4.45, 70.0625, 0.5]);
            gen.carts[0].set_motion([0.3, 0.0, 0.0]);
        }
        cart_tick();
        let (obs_pos, obs_vel) = {
            let gen = generated_content().lock().unwrap();
            (gen.carts[0].pos().unwrap(), gen.carts[0].motion())
        };
        assert!(obs_pos[0] <= 4.5101 && obs_vel[0] == 0.0, "obstacle should halt cart: pos={obs_pos:?} vel={obs_vel:?}");

        // Off-rail gravity & ground slowdown.
        {
            let mut gen = generated_content().lock().unwrap();
            gen.carts[0].set_pos([0.5, 70.05, 2.5]);
            gen.carts[0].set_motion([0.2, 0.0, 0.0]);
            gen.carts[0].set_on_ground(false);
        }
        cart_tick();
        cart_tick();
        cart_tick();
        let off = generated_content().lock().unwrap().carts[0].clone();
        assert!(off.on_ground(), "cart falling onto stone floor should set OnGround=true");
        assert!(off.motion()[0] <= 0.11, "ground slowdown should damp horizontal motion: {:?}", off.motion());
    }

    /// An end-ship `Elytra` frame reaches the runtime as a hanging entity: the store
    /// keeps the native save shape (block centre, `Facing` byte, framed stack), the
    /// spawn packet's object data is the facing and its yaw is `get2DDataValue * 90`,
    /// the metadata packet carries the stack at index 8, and a save/load keeps both.
    #[test]
    fn item_frame_payloads_materialize_spawn_and_survive_a_save() {
        let _guard = world_guard();
        let _snap = RuntimeSnapshot::take();
        forceloaded().lock().unwrap().insert((0, 0));
        refresh_hot();

        materialize_generation_payloads_inner(vec![
            (
                (4, 70, 4),
                crate::generation_payload::GenerationPayload::ItemFrame {
                    glow: false,
                    facing: 4,
                    item: Some(("minecraft:elytra".to_owned(), 1)),
                },
            ),
            (
                (5, 70, 4),
                crate::generation_payload::GenerationPayload::ItemFrame {
                    glow: true,
                    facing: 3,
                    item: None,
                },
            ),
        ]);
        let frames = generated_content().lock().unwrap().frames.clone();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].pos().unwrap(), [4.5, 70.5, 4.5]);
        assert_eq!(frames[0].facing().unwrap(), 4);
        assert!(!frames[0].glow());
        assert_eq!(
            frames[0].item(),
            Some(("minecraft:elytra".to_string(), 1)),
            "the framed elytra is stored, not just the entity"
        );
        assert!(frames[1].glow());
        assert_eq!(frames[1].item(), None);

        // `spawn_entity`: ... x/y/z doubles, pitch/yaw/head bytes, then the object data.
        let spawn = spawn_generated_frame(&frames[0]);
        let (eid, p) = read_varint_at(&spawn, 0);
        assert_eq!(eid, i64::from(frames[0].eid));
        let (etype, after) = read_varint_at(&spawn, p + 16);
        assert_eq!(etype, name_to_etype("item_frame").unwrap());
        let x = f64::from_be_bytes(spawn[after..after + 8].try_into().unwrap());
        assert_eq!(x, 4.5);
        let angles = after + 24;
        assert_eq!(spawn[angles], 0, "pitch");
        assert_eq!(spawn[angles + 1], angle(90.0) as u8, "west is 90 degrees");
        let (data, _) = read_varint_at(&spawn, angles + 3);
        assert_eq!(data, 4, "the facing travels as the object data");

        // `set_entity_data`: index 8 holds the `ItemStack`, index 9 the rotation.
        let meta = frame_meta_pkt(&frames[0]);
        let (meta_eid, p) = read_varint_at(&meta, 0);
        assert_eq!(meta_eid, i64::from(frames[0].eid));
        assert_eq!(meta[p], 8);
        let (kind, p) = read_varint_at(&meta, p + 1);
        assert_eq!(kind, 7, "ITEM_STACK serializer");
        let (count, p) = read_varint_at(&meta, p);
        assert_eq!(count, 1);
        let (item, p) = read_varint_at(&meta, p);
        assert_eq!(item, name_to_item("minecraft:elytra").unwrap());
        let (added, p) = read_varint_at(&meta, p);
        assert_eq!(added, 0, "no added components");
        let (removed, p) = read_varint_at(&meta, p);
        assert_eq!(removed, 0, "no removed components");
        assert_eq!(meta[p], 9, "index 9 is the item's rotation");
        let (kind, p) = read_varint_at(&meta, p + 1);
        assert_eq!(kind, 1, "INT serializer");
        assert_eq!(read_varint_at(&meta, p).0, 0, "rotation 0");
        assert_eq!(meta[meta.len() - 1], 0xff);

        // A client in range is spawned the frame and sent its stack.
        let (client, mut receiver) = effect_tests::client_pair();
        *client.pos.lock().unwrap() = (4.5, 70.0, 4.5, 0.0, 0.0, true);
        client.chunks.lock().unwrap().insert((0, 0));
        let client = Arc::new(client);
        clients().lock().unwrap().push(client.clone());
        interest_tick();
        let _ = client.sock.lock().unwrap().shutdown(Shutdown::Write);
        let (mut saw_spawn, mut saw_data, mut spawn_data) = (false, false, 0i64);
        while let Some((pid, body)) = read_packet(&mut receiver, Instant::now() + Duration::from_secs(1))
        {
            if pid == 0x01 {
                let (spawned, p) = read_varint_at(&body, 0);
                if spawned == i64::from(frames[0].eid) {
                    saw_spawn = true;
                    let (etype, after) = read_varint_at(&body, p + 16);
                    assert_eq!(etype, name_to_etype("item_frame").unwrap());
                    spawn_data = read_varint_at(&body, after + 24 + 3).0;
                }
            } else if pid == 0x58 {
                let (target, p) = read_varint_at(&body, 0);
                if target == i64::from(frames[0].eid) {
                    assert_eq!(body[p], 8);
                    saw_data = true;
                }
            }
        }
        assert!(saw_spawn && saw_data, "tracker sent spawn={saw_spawn} data={saw_data}");
        assert_eq!(spawn_data, 4, "the object data survives the tracker");

        // The generated-content section carries the frames across a save and load.
        let mut saved = Vec::new();
        save_world_into(&mut saved);
        generated_content().lock().unwrap().frames.clear();
        load_world_from(&saved).unwrap();
        let reloaded = generated_content().lock().unwrap().frames.clone();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded[0].pos().unwrap(), [4.5, 70.5, 4.5]);
        assert_eq!(reloaded[0].facing().unwrap(), 4);
        assert!(!reloaded[0].glow());
        assert_eq!(
            reloaded[0].item(),
            Some(("minecraft:elytra".to_string(), 1))
        );
        assert!(reloaded[1].glow());
        assert_eq!(reloaded[1].item(), None);
        assert_ne!(reloaded[0].uuid().unwrap(), [0u8; 16]);
        assert_ne!(
            reloaded[0].uuid().unwrap(),
            reloaded[1].uuid().unwrap(),
            "two frames keep distinct identities"
        );
    }

    #[test]
    fn spawner_entity_and_mob_payloads_materialize_and_tick_fails_closed() {
        let _guard = world_guard();
        let _snap = RuntimeSnapshot::take();
        forceloaded().lock().unwrap().insert((0, 0));
        refresh_hot();

        let (client, _receiver) = effect_tests::client_pair();
        *client.pos.lock().unwrap() = (0.5, 64.0, 0.5, 0.0, 0.0, true);
        *client.gm.lock().unwrap() = 0;
        clients().lock().unwrap().push(Arc::new(client));

        // Direct spawner::tick accepts the general 1.21.1 monster mobs and
        // still refuses an entity id outside them without panicking.
        struct DummySpawnerWorld;
        impl spawner::World for DummySpawnerWorld {
            fn active(&mut self, _: BlockPos, _: i32) -> bool { true }
            fn no_collision(&mut self, _: [f64; 3]) -> bool { true }
            fn rules(&mut self, _: [f64; 3], _: Option<spawner::LightRules>, _: &mut generation_math::Lcg) -> bool { true }
            fn nearby(&mut self, _: BlockPos, _: i32) -> usize { 0 }
            fn unobstructed(&mut self, _: [f64; 3]) -> bool { true }
            fn insert(&mut self, _: [f64; 3], _: f32) -> bool { true }
            fn effects(&mut self, _: BlockPos, _: [f64; 3]) {}
        }
        let mut rng = generation_math::Lcg::new(7);
        let mut dummy = DummySpawnerWorld;
        for id in ["minecraft:blaze", "minecraft:silverfish"] {
            let mut tag = generated_content::spawner_nbt_for((0, 64, 0), id);
            if let crate::nbt::Tag::Compound(ref mut map) = tag {
                map.insert(crate::nbt::NbtString::text("Delay"), crate::nbt::Tag::Short(0));
            }
            assert!(spawner::tick(&mut tag, (0, 64, 0), &mut dummy, &mut rng).is_ok());
        }
        let mut unsupported = generated_content::spawner_nbt_for((0, 64, 0), "minecraft:unknown_mob");
        if let crate::nbt::Tag::Compound(ref mut map) = unsupported {
            map.insert(crate::nbt::NbtString::text("Delay"), crate::nbt::Tag::Short(0));
        }
        assert!(spawner::tick(&mut unsupported, (0, 64, 0), &mut dummy, &mut rng).is_err());

        // Materialize SpawnerEntity + living structure Entity payloads.
        let mut payloads = vec![
            (
                (1, 64, 2),
                crate::generation_payload::GenerationPayload::SpawnerEntity("minecraft:blaze".to_owned()),
            ),
            (
                (3, 64, 4),
                crate::generation_payload::GenerationPayload::SpawnerEntity("minecraft:silverfish".to_owned()),
            ),
        ];
        let expected_mobs = [
            ("minecraft:elder_guardian", 80.0f32),
            ("minecraft:witch", 26.0),
            ("minecraft:cat", 10.0),
            ("minecraft:shulker", 30.0),
            ("minecraft:evoker", 24.0),
            ("minecraft:vindicator", 24.0),
            ("minecraft:allay", 20.0),
            ("minecraft:drowned", 20.0),
            ("minecraft:zombie_villager", 20.0),
            ("minecraft:villager", 20.0),
            ("minecraft:piglin", 16.0),
            ("minecraft:piglin_brute", 50.0),
            ("minecraft:hoglin", 40.0),
            ("minecraft:pillager", 24.0),
            ("minecraft:iron_golem", 100.0),
        ];
        for (i, (id, _)) in expected_mobs.iter().enumerate() {
            let fx = i as f64 + 0.5;
            payloads.push((
                (i as i32, 65, 5),
                crate::generation_payload::GenerationPayload::Entity {
                    id: (*id).to_owned(),
                    pos: [fx, 65.0, 5.5],
                    yaw: 90.0,
                },
            ));
        }
        materialize_generation_payloads_inner(payloads);

        // Verify spawners inserted and runtime spawner_tick() safely ignores them.
        let spawner_state = name_to_state("spawner").unwrap();
        assert_eq!(block_at(1, 64, 2), spawner_state);
        assert_eq!(block_at(3, 64, 4), spawner_state);
        {
            let mut gen = generated_content().lock().unwrap();
            for (pos, expected_id) in [((1, 64, 2), "minecraft:blaze"), ((3, 64, 4), "minecraft:silverfish")] {
                let tag = gen.spawners.get_mut(&pos).expect("materialized spawner NBT");
                let spawn_id = tag
                    .get("SpawnData")
                    .and_then(|d| d.get("entity"))
                    .and_then(|e| e.get("id"))
                    .and_then(crate::nbt::Tag::string);
                assert_eq!(spawn_id, Some(&crate::nbt::NbtString::text(expected_id)));
                if let crate::nbt::Tag::Compound(ref mut map) = tag {
                    map.insert(crate::nbt::NbtString::text("Delay"), crate::nbt::Tag::Short(0));
                }
            }
        }
        spawner_tick();

        // Verify all 15 living structure entities materialized with expected etype, pos, and HP.
        let ms = mobs().lock().unwrap();
        assert_eq!(ms.len(), expected_mobs.len());
        for (i, (id, expected_hp)) in expected_mobs.iter().enumerate() {
            let etype = name_to_etype(id).unwrap();
            let mob = ms.iter().find(|m| m.etype == etype).expect("materialized mob");
            assert_eq!(*mob.hp.lock().unwrap(), *expected_hp);
            let p = *mob.pos.lock().unwrap();
            assert_eq!(p, (i as f64 + 0.5, 65.0, 5.5, 90.0, 0.0));
        }
        drop(ms);

        // Respect MOB_CAP when mobs() is full.
        {
            let mut ms = mobs().lock().unwrap();
            while ms.len() < MOB_CAP {
                ms.push(Arc::new(Mob {
                    dim: DimId::Overworld,
                    eid: NEXT_EID.fetch_add(1, Ordering::SeqCst),
                    uuid: [1u8; 16],
                    etype: 124,
                    pos: Mutex::new((0.5, 65.0, 0.5, 0.0, 0.0)),
                    hp: Mutex::new(20.0),
                    last_hit: Mutex::new(Instant::now()),
                    hurt_at: Mutex::new(Instant::now()),
                    last_hurt_tick: Mutex::new(None),
                    vel: Mutex::new([0.0; 3]),
                    fuse: Mutex::new(0),
                    fire_ticks: Mutex::new(0),
                    love: Mutex::new(0),
                    air: Mutex::new(60),
                }));
            }
        }
        materialize_generation_payloads_inner(vec![(
            (0, 65, 0),
            crate::generation_payload::GenerationPayload::Entity {
                id: "minecraft:witch".to_owned(),
                pos: [0.5, 65.0, 0.5],
                yaw: 0.0,
            },
        )]);
        assert_eq!(mobs().lock().unwrap().len(), MOB_CAP);
    }
}

#[cfg(test)]
mod dim_block_entity_invalidation_tests {
    use super::*;

    struct Restore {
        store: LiveBlockEntityStore,
        cells: Vec<(DimBlockPos, u32)>,
        signs: Vec<SignKey>,
    }

    impl Restore {
        fn take() -> Self {
            Self {
                store: std::mem::take(&mut *live_block_entities().lock().unwrap()),
                cells: Vec::new(),
                signs: Vec::new(),
            }
        }
    }

    impl std::ops::Drop for Restore {
        fn drop(&mut self) {
            for &((dim, x, y, z), state) in self.cells.iter().rev() {
                set_block_dim(dim, x, y, z, state);
            }
            if !self.signs.is_empty() {
                let mut store = signs().lock().unwrap();
                for key in &self.signs {
                    store.remove(key);
                }
            }
            *live_block_entities().lock().unwrap() = std::mem::take(&mut self.store);
        }
    }

    #[test]
    fn dim_block_rewrite_drops_live_block_entity_immediately_but_keeps_state_writes() {
        let _guard = world_guard();
        let mut restore = Restore::take();
        let dim = DimId::Nether;
        let (x, y, z) = (93_500, 220, 93_500);
        let key = (dim, x, y, z);
        let reg = breg().unwrap();
        let banner = item_to_state(name_to_item("red_banner").unwrap()).unwrap();

        restore.cells.push((key, block_at_dim(dim, x, y, z)));
        set_block_dim(dim, x, y, z, banner);
        assert!(live_block_entity_snapshot(key).is_some());

        // A payload a fresh placement cannot produce.
        let layer = block_entity_compound([
            ("pattern", block_entity_string("minecraft:stripe_center")),
            ("color", block_entity_string("blue")),
        ]);
        let patterns = block_entity_list(10, vec![layer]);
        let banner_tag = block_entity_compound([
            ("id", block_entity_string("minecraft:banner")),
            ("patterns", patterns.clone()),
        ]);
        assert!(restore_live_block_entity(key, &banner_tag));

        // A same-block property write keeps the entity and its payload.
        let rotated = reg.with_prop(banner, "rotation", "3");
        assert_ne!(rotated, banner);
        set_block_dim(dim, x, y, z, rotated);
        let (_, tag) = live_block_entity_snapshot(key).unwrap();
        assert_eq!(tag.get("patterns"), Some(&patterns));

        // Sign text follows the same identity rule on this write path.
        for side in [0u8, 1] {
            let sign_key = (dim, x, y, z, side);
            restore.signs.push(sign_key);
            signs().lock().unwrap().insert(
                sign_key,
                ["text".to_owned(), String::new(), String::new(), String::new()],
            );
        }

        // Breaking the block retires the entity and the sign text at once —
        // no discovery scan required.
        set_block_dim(dim, x, y, z, 0);
        assert!(!live_block_entities()
            .lock()
            .unwrap()
            .entries
            .contains_key(&key));
        let sign_store = signs().lock().unwrap();
        assert!(!sign_store.contains_key(&(dim, x, y, z, 0)));
        assert!(!sign_store.contains_key(&(dim, x, y, z, 1)));
        drop(sign_store);

        // Re-placing the same block must not resurrect the stale payload.
        set_block_dim(dim, x, y, z, banner);
        let (_, fresh) = live_block_entity_snapshot(key).unwrap();
        assert!(fresh.get("patterns").is_none());
    }
}

#[cfg(test)]
mod wire_ops_tests {
    use super::*;

    /// Poison-tolerant lock: the ratchet forbids growing `.lock().unwrap()`,
    /// tests included.
    fn wire_lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wire_ops_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("mc-spine-wire-ops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Disarm the journal: the append path returns to its zero-cost early
    /// return and the slot is emptied so a later `dwal_boot_recover_and_arm`
    /// starts clean.
    fn wire_ops_dwal_disarm() {
        DWAL_ON.store(false, Ordering::Relaxed);
        *wire_lock(live_dwal()) = None;
    }

    /// Every real `game_tick` must land in the shared profiler and in the
    /// OPS-2 lag-compensation feed, in matching units off one `Instant`.
    #[test]
    fn wire_ops_game_tick_feeds_profiler_and_lag_compensation() {
        {
            // Another test may leave the controller frozen; a skipped tick
            // would feed nothing.
            let mut ctl = wire_lock(tick_controller());
            ctl.frozen = false;
            ctl.step_remaining = 0;
            ctl.sprint_remaining = 0;
        }
        // `ring_len` saturates at capacity once the shared ring fills, so the
        // signal a tick landed is `ring_next` advancing by exactly one slot.
        let before = wire_lock(live_hotspot_profiler()).ring_next;
        game_tick(|_, tick| tick());
        let last_us = {
            let profiler = wire_lock(live_hotspot_profiler());
            assert_eq!(
                profiler.ring_next,
                (before + 1) % PROFILER_RING_CAPACITY
            );
            profiler.ring[(profiler.ring_next + PROFILER_RING_CAPACITY - 1)
                % PROFILER_RING_CAPACITY]
        };
        assert_eq!(ac_last_tick_ms(), last_us / 1000);
    }

    /// The play loop feeds `ac_note_player_rtt_ms` on every keepalive reply;
    /// the readback is what the movement envelope scales with.
    #[test]
    fn wire_ops_keepalive_rtt_feeds_lag_compensation() {
        ac_note_player_rtt_ms(-4242, 250);
        assert_eq!(ac_player_rtt_ms(-4242), 250);
    }

    /// Opt-in `MC_SPINE_WAL_PATH`: a live edit lands in the journal, a
    /// crash-recovery replays it into the live overlay, and a checkpointed
    /// full save compacts the log to its anchor marker.
    #[test]
    fn wire_ops_dwal_append_recover_replay_and_compact() {
        let dir = wire_ops_dir("dwal");
        let wal_path = dir.join("delta.wal");
        let save_dir = dir.join("save");
        std::fs::create_dir_all(&save_dir).unwrap();
        std::env::set_var("MC_SPINE_WAL_PATH", &wal_path);
        wire_ops_dwal_disarm();

        let dim = DimId::Overworld;
        let (x, y, z) = (3_000_001, 80, -2_999_999);
        let chunk_key = (dim, x >> 4, z >> 4);
        let state = name_to_state("stone").unwrap_or(1);
        assert_ne!(block_at_dim(dim, x, y, z), state);

        // Arm through the same entry point boot uses.
        dwal_boot_recover_and_arm();
        assert!(dwal_enabled());

        // A live edit lands in the journal immediately.
        set_block_dim(dim, x, y, z, state);
        let report = recover_after_checkpoint(&wal_path).unwrap();
        assert!(report.records.contains(&DeltaWalRecord::BlockSet {
            dim,
            x,
            y,
            z,
            state,
        }));

        // Crash: journal handle gone, the overlay edit lost with it.
        wire_ops_dwal_disarm();
        wire_lock(world()).remove(&chunk_key);
        assert!(overlay_block_dim(dim, x, y, z).is_none());

        // Boot recovery replays the journaled edit into the live overlay.
        dwal_boot_recover_and_arm();
        assert_eq!(block_at_dim(dim, x, y, z), state);

        // A checkpointed full save compacts the log to its anchor marker.
        dwal_save_checkpointed(|snapshot| snapshot.write(&save_dir)).unwrap();
        let report = recover_after_checkpoint(&wal_path).unwrap();
        assert!(report.checkpoint_seq.is_some());
        assert!(report.records.is_empty());

        // Post-checkpoint edits keep journaling on the rotated sequence.
        set_block_dim(dim, x, y, z, 0);
        let report = recover_after_checkpoint(&wal_path).unwrap();
        assert!(report.records.contains(&DeltaWalRecord::BlockSet {
            dim,
            x,
            y,
            z,
            state: 0,
        }));

        // Leave nothing behind: journal off, env cleared, cells dropped.
        wire_ops_dwal_disarm();
        std::env::remove_var("MC_SPINE_WAL_PATH");
        wire_lock(world()).remove(&chunk_key);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An invalid `MC_SPINE_WAL_PATH` must behave like an unset one — no
    /// journal, no writes, no crash.
    #[test]
    fn wire_ops_dwal_rejects_traversal_path() {
        let dir = wire_ops_dir("dwal-bad");
        let saved = std::env::var_os("MC_SPINE_WAL_PATH");
        std::env::set_var("MC_SPINE_WAL_PATH", dir.join("..").join("escape.wal"));
        assert!(dwal_path_from_env().is_none());
        match saved {
            Some(v) => std::env::set_var("MC_SPINE_WAL_PATH", v),
            None => std::env::remove_var("MC_SPINE_WAL_PATH"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------- Lane P8a: parity tests ----------------

#[cfg(test)]
mod p8a_parity_tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Poison-tolerant lock: the ratchet forbids growing `.lock().unwrap()`,
    /// tests included.
    fn p8a_lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writes `st` into the live overlay for `dim` at `(x, y, z)`, restoring the
    /// previous cell (or its absence) on drop.
    struct DimCells(Vec<(DimId, i32, i32, i32, Option<u32>)>);
    impl DimCells {
        fn new() -> Self {
            Self(Vec::new())
        }
        fn set(&mut self, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
            let key = (dim, x >> 4, z >> 4);
            let cell = local_idx(x, y, z);
            let prev = {
                let mut w = p8a_lock(world());
                let chunk = w.entry(key).or_default();
                let prev = chunk.get(&cell).copied();
                chunk.insert(cell, st);
                prev
            };
            self.0.push((dim, x, y, z, prev));
        }
    }
    impl std::ops::Drop for DimCells {
        fn drop(&mut self) {
            let mut w = p8a_lock(world());
            for (dim, x, y, z, prev) in self.0.drain(..).rev() {
                if let Some(chunk) = w.get_mut(&(dim, x >> 4, z >> 4)) {
                    match prev {
                        Some(st) => {
                            chunk.insert(local_idx(x, y, z), st);
                        }
                        None => {
                            chunk.remove(&local_idx(x, y, z));
                        }
                    }
                }
            }
        }
    }

    /// #86: a comparator analog read resolves the container in the comparator's
    /// own dimension — same coordinates, different contents per dimension, and
    /// the Nether world view must see the Nether chest.
    #[test]
    fn p8a_parity_comparator_container_read_uses_own_dimension() {
        crate::test_support::require_vanilla_data();
        let Some(reg) = breg() else { return };
        let _guard = world_guard();
        let (x, y, z) = (2_500_003, 70, -2_500_001);
        let chest = reg.state_of("minecraft:chest", None);
        if reg.name_of(chest) != "minecraft:chest" {
            return;
        }
        let mut cells = DimCells::new();
        cells.set(DimId::Nether, x, y, z, chest);
        cells.set(DimId::Overworld, x, y, z, chest);
        let stack: Stack = (803, 64, Vec::new());
        let key_nether = ContainerKey::Block((DimId::Nether, x, y, z));
        let key_ow = ContainerKey::Block((DimId::Overworld, x, y, z));
        let prev_nether = {
            let mut chests = p8a_lock(chests());
            chests.insert(
                key_nether,
                HashMap::from([
                    (0i16, stack.clone()),
                    (1, stack.clone()),
                    (2, stack),
                ]),
            )
        };
        let prev_ow = p8a_lock(chests()).insert(key_ow, HashMap::new());
        let mut state = LiveRedstoneState {
            comparator_outputs: BTreeMap::new(),
            torches: BTreeMap::new(),
            ticks: BinaryHeap::new(),
            pending: BTreeMap::new(),
            sequence: 0,
        };
        // Three full stacks in 27 slots: floor(14 * (3 / 27)) + 1 = 2.
        let nether_signal = {
            let world = LiveRedstoneWorld {
                dim: DimId::Nether,
                reg,
                state: &mut state,
                now: 0,
                reads: std::cell::RefCell::new(BTreeMap::new()),
                writes: BTreeMap::new(),
                dispensers: Vec::new(),
            };
            world.analog_signal(RsPos(x, y, z))
        };
        // The empty Overworld chest at the same coordinates reads 0 — only a
        // dimension-scoped lookup keeps the two apart.
        let ow_signal = {
            let world = LiveRedstoneWorld {
                dim: DimId::Overworld,
                reg,
                state: &mut state,
                now: 0,
                reads: std::cell::RefCell::new(BTreeMap::new()),
                writes: BTreeMap::new(),
                dispensers: Vec::new(),
            };
            world.analog_signal(RsPos(x, y, z))
        };
        {
            let mut chests = p8a_lock(chests());
            match prev_nether {
                Some(m) => {
                    chests.insert(key_nether, m);
                }
                None => {
                    chests.remove(&key_nether);
                }
            }
            match prev_ow {
                Some(m) => {
                    chests.insert(key_ow, m);
                }
                None => {
                    chests.remove(&key_ow);
                }
            }
        }
        assert_eq!(nether_signal, Some(2), "Nether comparator reads Nether chest");
        assert_eq!(ow_signal, Some(0), "Overworld comparator reads Overworld chest");
    }

    /// #104: the chunk heightmap compound carries both `MOTION_BLOCKING` and
    /// `WORLD_SURFACE`, each the 37-long 9-bit non-straddling packed array
    /// Vanilla 1.21.1 writes.
    #[test]
    fn p8a_parity_heightmaps_emit_motion_blocking_and_world_surface() {
        let mut mb = [0i32; 256];
        let mut ws = [0i32; 256];
        mb[0] = 70;
        mb[255] = 100;
        ws[0] = 80;
        ws[255] = 90;
        let nbt = heightmaps_nbt_dim(&mb, &ws, -64);
        fn entry(nbt: &[u8], at: usize) -> (&str, Vec<u64>, usize) {
            assert_eq!(nbt[at], 12, "TAG_Long_Array");
            let name_len = u16::from_be_bytes([nbt[at + 1], nbt[at + 2]]) as usize;
            let name = std::str::from_utf8(&nbt[at + 3..at + 3 + name_len]).unwrap();
            let mut cur = at + 3 + name_len;
            let count = i32::from_be_bytes(nbt[cur..cur + 4].try_into().unwrap());
            assert_eq!(count, 37, "a Vanilla heightmap is exactly 37 longs");
            cur += 4;
            let longs: Vec<u64> = (0..37)
                .map(|i| u64::from_be_bytes(nbt[cur + 8 * i..cur + 8 * i + 8].try_into().unwrap()))
                .collect();
            (name, longs, cur + 37 * 8)
        }
        assert_eq!(nbt[0], 10, "anonymous TAG_Compound");
        let (n1, l1, at) = entry(&nbt, 1);
        let (n2, l2, at) = entry(&nbt, at);
        assert_eq!(n1, "MOTION_BLOCKING");
        assert_eq!(n2, "WORLD_SURFACE");
        assert_eq!(nbt[at], 0, "compound terminator");
        assert_eq!(at + 1, nbt.len(), "nothing trails the compound");
        let val = |longs: &[u64], i: usize| (longs[i / 7] >> ((i % 7) * 9)) & 0x1ff;
        assert_eq!(val(&l1, 0), 134); // 70 - (-64)
        assert_eq!(val(&l1, 255), 164); // 100 - (-64)
        assert_eq!(val(&l2, 0), 144); // 80 - (-64)
        assert_eq!(val(&l2, 255), 154); // 90 - (-64)
    }

    /// Profiler unification: `game_tick` records into the very same
    /// `ServerHotspotProfiler` `/profile` reads — one `OnceLock`, not two.
    #[test]
    fn p8a_parity_game_tick_profiler_is_the_profiled_instance() {
        {
            // Another test may leave the controller frozen; a skipped tick
            // would feed nothing.
            let mut ctl = p8a_lock(tick_controller());
            ctl.frozen = false;
            ctl.step_remaining = 0;
            ctl.sprint_remaining = 0;
        }
        // `ring_len` saturates at capacity, so the shared-ring signal is
        // `ring_next` advancing exactly one slot per recorded tick.
        let before = p8a_lock(live_hotspot_profiler()).ring_next;
        game_tick(|_, tick| tick());
        assert_eq!(
            p8a_lock(live_hotspot_profiler()).ring_next,
            (before + 1) % PROFILER_RING_CAPACITY,
            "a real game_tick must land in the /profile profiler"
        );
    }

    /// #124: `game_tick` stages follow Vanilla `ServerLevel.tick` order —
    /// scheduled block ticks, fluid ticks, raids, the chunk pass (entities,
    /// block entities, `tickChunk` random ticks), then queued block events.
    #[test]
    fn p8a_parity_tick_phases_follow_vanilla_order() {
        {
            let mut ctl = p8a_lock(tick_controller());
            ctl.frozen = false;
            ctl.step_remaining = 0;
            ctl.sprint_remaining = 0;
        }
        let mut names: Vec<&'static str> = Vec::new();
        game_tick(|name, _| names.push(name));
        let at = |name: &str| {
            names
                .iter()
                .position(|&n| n == name)
                .unwrap_or_else(|| panic!("stage {name} missing from {names:?}"))
        };
        assert!(at("scheduled_blocks") < at("generated_fluids"));
        assert!(at("redstone") < at("villager_raid"));
        assert!(at("generated_fluids") < at("water"));
        assert!(at("water") < at("villager_raid"));
        assert!(at("villager_raid") < at("drops"));
        assert!(at("villager_raid") < at("mobs"));
        assert!(at("drops") < at("mobs"));
        assert!(at("mobs") < at("furnaces"));
        assert!(at("furnaces") < at("random_ticks"));
        assert!(at("hoppers") < at("random_ticks"));
        assert!(at("random_ticks") < at("block_updates"));
        assert!(at("block_updates") < at("live_mods"));
    }

    /// #120: `villager_raid` is a real `game_tick` stage — omen conversion and
    /// raid progression run every server tick — gated by the live
    /// `disableRaids` gamerule atomic.
    #[test]
    fn p8a_parity_raid_stage_is_wired_and_disable_gated() {
        {
            let mut ctl = p8a_lock(tick_controller());
            ctl.frozen = false;
            ctl.step_remaining = 0;
            ctl.sprint_remaining = 0;
        }
        let mut names: Vec<&'static str> = Vec::new();
        game_tick(|name, _| names.push(name));
        assert!(
            names.contains(&"villager_raid"),
            "raid trigger and progression must tick inside game_tick"
        );
        let saved = GR_DISABLE_RAIDS.swap(1, Ordering::Relaxed);
        assert!(gr_disable_raids());
        GR_DISABLE_RAIDS.store(0, Ordering::Relaxed);
        assert!(!gr_disable_raids());
        GR_DISABLE_RAIDS.store(saved, Ordering::Relaxed);
    }
}

/// Lane W3 live-wire coverage: the four seams this lane connected — the
/// comparator analog read through `live_comparator_signal_at`, the W-BE
/// registry through the `block_entities` `game_tick` stage, the live policy
/// into the join fair-play signal and play frame bound, and the P8b-2
/// `award_stats` encoder behind C2S `ClientCommand` action 1.
#[cfg(test)]
mod w3_live_wire_tests {
    use super::*;
    use std::collections::{BTreeMap, BinaryHeap};
    use std::env;

    /// Records an overlay block write so the original cell (set or absent)
    /// goes back exactly — the same discipline `DimCells` uses in
    /// `p8a_parity_tests`, which lives in a different test module.
    struct LiveCells(Vec<(DimId, i32, i32, i32, Option<u32>)>);
    impl LiveCells {
        fn new() -> Self {
            Self(Vec::new())
        }
        fn set(&mut self, dim: DimId, x: i32, y: i32, z: i32, st: u32) {
            let key = (dim, x >> 4, z >> 4);
            let cell = local_idx(x, y, z);
            let prev = {
                let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
                let chunk = w.entry(key).or_default();
                let prev = chunk.get(&cell).copied();
                chunk.insert(cell, st);
                prev
            };
            self.0.push((dim, x, y, z, prev));
        }
    }
    impl std::ops::Drop for LiveCells {
        fn drop(&mut self) {
            let mut w = world().lock().unwrap_or_else(|e| e.into_inner());
            for (dim, x, y, z, prev) in self.0.drain(..).rev() {
                if let Some(chunk) = w.get_mut(&(dim, x >> 4, z >> 4)) {
                    match prev {
                        Some(st) => {
                            chunk.insert(local_idx(x, y, z), st);
                        }
                        None => {
                            chunk.remove(&local_idx(x, y, z));
                        }
                    }
                }
            }
        }
    }

    fn redstone_world<'a>(
        state: &'a mut LiveRedstoneState,
        dim: DimId,
        reg: &'static BReg,
    ) -> LiveRedstoneWorld<'a> {
        LiveRedstoneWorld {
            dim,
            reg,
            state,
            now: 0,
            reads: std::cell::RefCell::new(BTreeMap::new()),
            writes: BTreeMap::new(),
            dispensers: Vec::new(),
        }
    }

    fn empty_redstone_state() -> LiveRedstoneState {
        LiveRedstoneState {
            comparator_outputs: BTreeMap::new(),
            torches: BTreeMap::new(),
            ticks: BinaryHeap::new(),
            pending: BTreeMap::new(),
            sequence: 0,
        }
    }

    /// Runs one `game_tick` but only the named stage — proof the stage is
    /// registered in the live tick loop, without letting the whole world tick.
    fn run_tick_stage(stage: &'static str) {
        {
            let mut ctl = tick_controller()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            ctl.frozen = false;
            ctl.step_remaining = 0;
            ctl.sprint_remaining = 0;
        }
        let mut ran = false;
        game_tick(|name, tick| {
            if name == stage {
                tick();
                ran = true;
            }
        });
        assert!(ran, "game_tick must schedule the {stage} stage");
    }

    /// #1: `LiveRedstoneWorld::analog_signal` answers through
    /// `live_comparator_signal_at` — a W-BE jukebox with a record reads its
    /// disc's comparator output while a bare position still reads nothing.
    #[test]
    fn w3_live_wire_comparator_reads_w_be_signal() {
        crate::test_support::require_vanilla_data();
        let Some(reg) = breg() else { return };
        let _guard = world_guard();
        let dim = DimId::Overworld;
        let pos = (2_530_001, 200, -2_530_001);
        let jukebox_state = reg.state_of("minecraft:jukebox", None);
        let mut cells = LiveCells::new();
        cells.set(dim, pos.0, pos.1, pos.2, jukebox_state);
        assert!(register_world_block_entity(
            dim,
            pos,
            VanillaBlockEntity::Jukebox(JukeboxBlockEntity {
                record_item: Some("minecraft:music_disc_cat".into()),
                ticks_since_song_started: 0,
            }),
        )
        .is_none());
        let mut state = empty_redstone_state();
        let (loaded, empty_at) = {
            let world = redstone_world(&mut state, dim, reg);
            (
                world.analog_signal(RsPos(pos.0, pos.1, pos.2)),
                world.analog_signal(RsPos(pos.0 + 8, pos.1, pos.2)),
            )
        };
        remove_world_block_entity(dim, pos);
        // `music_disc_cat`'s jukebox_song `comparator_output` is 2; an absent
        // block entity must stay `None` (vanilla "no comparator source").
        assert_eq!(loaded, Some(2));
        assert_eq!(empty_at, None);
    }

    /// #2a: the `block_entities` stage of `game_tick` runs
    /// `tick_world_block_entities` live — a lit campfire's finished slot
    /// commits as a cooked-item entity drop, and the jukebox's song timer
    /// advances in the same pass.
    #[test]
    fn w3_live_wire_game_tick_campfire_drop_and_jukebox_advance() {
        crate::test_support::require_vanilla_data();
        let Some(reg) = breg() else { return };
        let _guard = world_guard();
        let dim = DimId::Overworld;
        let fire_pos = (2_531_001, 200, -2_531_001);
        let box_pos = (2_531_009, 200, -2_531_009);
        let campfire = reg.state_of("minecraft:campfire", None);
        let mut cells = LiveCells::new();
        cells.set(
            dim,
            fire_pos.0,
            fire_pos.1,
            fire_pos.2,
            reg.with_prop(campfire, "lit", "true"),
        );
        let mut fire = CampfireBlockEntity::default();
        assert!(fire.place("beef", 1).is_some());
        assert!(register_world_block_entity(
            dim,
            fire_pos,
            VanillaBlockEntity::Campfire(fire),
        )
        .is_none());
        assert!(register_world_block_entity(
            dim,
            box_pos,
            VanillaBlockEntity::Jukebox(JukeboxBlockEntity {
                record_item: Some("minecraft:music_disc_cat".into()),
                ticks_since_song_started: 0,
            }),
        )
        .is_none());
        let base_drops = drops().lock().unwrap_or_else(|e| e.into_inner()).len();
        run_tick_stage("block_entities");
        let cooked = name_to_item("minecraft:cooked_beef").expect("item id");
        let dropped = drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .skip(base_drops)
            .any(|drop| {
                drop.item == cooked
                    && drop.cnt == 1
                    && (drop.x - (f64::from(fire_pos.0) + 0.5)).abs() < f64::EPSILON
                    && (drop.z - (f64::from(fire_pos.2) + 0.5)).abs() < f64::EPSILON
            });
        drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .truncate(base_drops);
        let VanillaBlockEntity::Jukebox(jukebox) =
            remove_world_block_entity(dim, box_pos).expect("jukebox")
        else {
            panic!("jukebox must still be registered")
        };
        let VanillaBlockEntity::Campfire(fire) =
            remove_world_block_entity(dim, fire_pos).expect("campfire")
        else {
            panic!("campfire must still be registered")
        };
        assert!(dropped, "cooked beef must appear as a live item drop");
        assert!(fire.slots.iter().all(Option::is_none));
        assert_eq!(jukebox.ticks_since_song_started, 1);
    }

    /// #2b: the same stage grants a beacon's effect pulse — a level-1 pyramid
    /// under a live beacon must reach a nearby survival client through
    /// `w_be_grant_effect` (and the 0x76 packet path).
    #[test]
    fn w3_live_wire_game_tick_beacon_grants_effect() {
        crate::test_support::require_vanilla_data();
        let Some(reg) = breg() else { return };
        let _guard = world_guard();
        let dim = DimId::Overworld;
        let pos = (2_532_001, 250, -2_532_001);
        let iron = reg.state_of("minecraft:iron_block", None);
        let beacon_state = reg.state_of("minecraft:beacon", None);
        let mut cells = LiveCells::new();
        for dx in -1..=1 {
            for dz in -1..=1 {
                cells.set(dim, pos.0 + dx, pos.1 - 1, pos.2 + dz, iron);
            }
        }
        cells.set(dim, pos.0, pos.1, pos.2, beacon_state);
        assert!(register_world_block_entity(
            dim,
            pos,
            VanillaBlockEntity::Beacon(BeaconBlockEntity {
                levels: 1,
                primary_effect: Some("minecraft:haste".into()),
                secondary_effect: None,
            }),
        )
        .is_none());
        let (mut client, _receiver) = crate::effect_tests::client_pair();
        client.eid = 0x7A33;
        *client.pos.lock().unwrap_or_else(|e| e.into_inner()) =
            (f64::from(pos.0) + 1.5, f64::from(pos.1) + 2.0, f64::from(pos.2) + 1.5, 0.0, 0.0, true);
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::new(client));
        // The beacon fires on `world_tick % 80 == 0`; game_tick advances
        // TICK_IDX once before the stage runs.
        let saved_tick = TICK_IDX.swap(79, Ordering::Relaxed);
        run_tick_stage("block_entities");
        TICK_IDX.store(saved_tick, Ordering::Relaxed);
        let mut cls = clients().lock().unwrap_or_else(|e| e.into_inner());
        let client = cls
            .iter()
            .find(|c| c.eid == 0x7A33)
            .cloned()
            .expect("client still connected");
        cls.retain(|c| c.eid != 0x7A33);
        drop(cls);
        remove_world_block_entity(dim, pos);
        let effects = client.effs.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            effects
                .iter()
                .any(|eff| eff.id == 2 && eff.amp == 0 && eff.left > 0),
            "haste (effect id 2) must be granted by the live beacon pulse"
        );
    }

    /// #3: `live_server_policy()` joins `MC_SPINE_FAIR_PLAY` at the join
    /// decision — `unrestricted` sends nothing, `fair_play` sends the reset
    /// plus selective deny codes, `disabled` sends the full lock — and
    /// `max_frame_mib` scales the decompressed play-frame bound.
    #[test]
    fn w3_live_wire_policy_minimap_signal_and_frame_bound() {
        let _guard = world_guard();
        const ENVS: [&str; 3] = [
            "MC_SPINE_FAIR_PLAY",
            "MC_SPINE_ALLOW_CAVE_MAP",
            "MC_SPINE_ALLOW_ENTITY_RADAR",
        ];
        let saved_env: Vec<Option<std::ffi::OsString>> =
            ENVS.iter().map(env::var_os).collect();
        let saved_policy = live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for name in ENVS {
            env::remove_var(name);
        }
        let restore = |saved_env: &[Option<std::ffi::OsString>],
                       saved_policy: &LiveServerPolicy| {
            *live_server_policy()
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = saved_policy.clone();
            for (name, value) in ENVS.iter().zip(saved_env) {
                match value {
                    Some(v) => env::set_var(name, v),
                    None => env::remove_var(name),
                }
            }
        };
        // Default: unrestricted + env unset → vanilla-permissive, zero codes.
        let policy = map_fair_play_policy();
        assert!(!policy.fair_play);
        assert!(map_fair_play_join_chats(&policy).is_empty());
        // fair_play with all features allowed → the reset signal only.
        live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .minimap_mode = "fair_play".to_owned();
        let policy = map_fair_play_policy();
        assert!(policy.fair_play);
        assert_eq!(policy.control, "§r§e§s§e§t§x§a§e§r§o");
        assert_eq!(map_fair_play_join_chats(&policy).len(), 1);
        // Denying mob radar adds the selective code after the reset.
        live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .allow_mob_radar = false;
        let policy = map_fair_play_policy();
        assert_eq!(
            policy.control,
            "§r§e§s§e§t§x§a§e§r§o§n§o§e§n§t§i§t§y§r§a§d§a§r§x§a§e§r§o"
        );
        // Denying everything under fair_play collapses to the full lock.
        live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .allow_cave_map = false;
        let policy = map_fair_play_policy();
        assert_eq!(policy.control, "§f§a§i§r§x§a§e§r§o");
        // `disabled` is the full lock on its own.
        {
            let mut p = live_server_policy()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            p.minimap_mode = "disabled".to_owned();
            p.allow_cave_map = true;
            p.allow_mob_radar = true;
        }
        let policy = map_fair_play_policy();
        assert_eq!(policy.control, "§f§a§i§r§x§a§e§r§o");
        // The env flag still forces the lock under an unrestricted policy.
        {
            let mut p = live_server_policy()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            p.minimap_mode = "unrestricted".to_owned();
        }
        env::set_var("MC_SPINE_FAIR_PLAY", "1");
        let policy = map_fair_play_policy();
        assert_eq!(policy.control, "§f§a§i§r§x§a§e§r§o");
        env::remove_var("MC_SPINE_FAIR_PLAY");
        // `max_frame_mib` scales the decompressed frame bound.
        live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .max_frame_mib = 16;
        assert_eq!(live_play_frame_bound(), (16usize << 20) - 1);
        live_server_policy()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .max_frame_mib = 2;
        assert_eq!(live_play_frame_bound(), ingress_guard::MAX_PLAY_FRAME);
        restore(&saved_env, &saved_policy);
    }

    /// #4: the `award_stats` payload C2S `ClientCommand` action 1 now encodes
    /// through the P8b-2 `encode_award_stats_0x04` codec — byte-identical to
    /// the stats-request encoder it replaced, and it decodes through the same
    /// live `award_stats` wire decoder.
    #[test]
    fn w3_live_wire_award_stats_uses_p8b2_codec() {
        let mut stats = PlayerStats::new();
        stats.increment(StatCategory::Custom, 42, 7);
        stats.increment(StatCategory::Mined, 3, 11);
        stats.increment(StatCategory::Dropped, 9, 2);
        // The exact mapping the live 0x09 handler applies to the stat book.
        let entries: Vec<(u8, i32, i32)> = stats
            .counts
            .iter()
            .map(|(&(category, stat_id), &value)| {
                (category.id() as u8, stat_id, value)
            })
            .collect();
        let payload = encode_award_stats_0x04(&entries);
        assert_eq!(
            payload,
            handle_client_stats_request(&stats),
            "the live handler must emit the same award_stats payload"
        );
        let decoded =
            decode_award_stats(&mut &payload[..]).expect("valid 0x04 payload");
        assert_eq!(decoded.counts, stats.counts);
    }
}

#[cfg(test)]
mod xplat2_tests {
    use super::os_fill_random;

    #[test]
    fn xplat2_os_fill_random_produces_non_zero_entropy() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        assert!(os_fill_random(&mut a), "OS entropy source must fill");
        assert!(os_fill_random(&mut b), "OS entropy source must fill");
        assert!(
            a.iter().any(|&x| x != 0),
            "filled buffer must not stay zeroed"
        );
        assert_ne!(a, b, "two consecutive draws must differ");
    }
}

#[cfg(test)]
mod vanilla_leash_tests {
    use super::*;

    fn vl_deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn vl_spawn_mob(etype: i64, pos: (f64, f64, f64)) -> Arc<Mob> {
        let mob = new_live_mob(etype, DimId::Overworld, (pos.0, pos.1, pos.2, 0.0, 0.0), 8.0, None);
        mobs()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(mob.clone());
        mob
    }

    /// A registered, tracking client: in `clients()` with the entity marked
    /// `seen` so the broadcast helpers write onto its socket pair.
    fn vl_client(eid: i32, pos: (f64, f64, f64)) -> (Arc<Client>, TcpStream) {
        let (mut client, peer) = effect_tests::client_pair();
        client.eid = eid;
        client.pos = Mutex::new((pos.0, pos.1, pos.2, 0.0, 0.0, true));
        let client = Arc::new(client);
        clients()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(client.clone());
        (client, peer)
    }

    struct VlCleanup {
        eids: Vec<i32>,
        client_eids: Vec<i32>,
    }
    impl std::ops::Drop for VlCleanup {
        fn drop(&mut self) {
            for &eid in &self.eids {
                mobs()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|m| m.eid != eid);
                live_mob_states()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&eid);
                mob_custom_names()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&eid);
            }
            entity_tethers()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            leash_knots()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            drops()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            let mut cls = clients().lock().unwrap_or_else(|e| e.into_inner());
            for &eid in &self.client_eids {
                cls.retain(|c| c.eid != eid);
            }
        }
    }

    fn vl_read(peer: &mut TcpStream) -> (i64, Vec<u8>) {
        read_packet(peer, vl_deadline()).expect("expected a packet on the wire")
    }

    #[test]
    fn vanilla_leash_set_entity_link_wire_bytes() {
        // Protocol 767 `set_entity_link` (`0x59`): two big-endian i32s,
        // attached then holding; -1 is the detach form.
        assert_eq!(
            set_entity_link_pkt(0x0102_0304, 0x0506_0708),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(
            set_entity_link_pkt(0x0102_0304, -1),
            vec![1, 2, 3, 4, 0xff, 0xff, 0xff, 0xff]
        );
    }

    #[test]
    fn vanilla_leash_attach_pull_snap_and_drop() {
        let _world_guard = world_guard();
        let mob = vl_spawn_mob(22, (0.0, 70.0, 8.0)); // cow, 8 blocks out
        let (client, mut peer) = vl_client(42420, (0.0, 70.0, 0.0));
        client
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(mob.eid);
        let _cleanup = VlCleanup {
            eids: vec![mob.eid],
            client_eids: vec![client.eid],
        };
        let lead = name_to_item("lead").unwrap();
        client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, (lead, 2, Vec::new()));

        // Right-click with a lead: tether to the player, wire 0x59(mob, player),
        // one lead consumed in survival (the slot resync packet trails it).
        assert!(leash_name_tag_interact(&client, mob.eid, 36));
        assert_eq!(
            vl_read(&mut peer),
            (0x59, set_entity_link_pkt(mob.eid, client.eid))
        );
        let (id, _) = vl_read(&mut peer); // 0x15 slot resync for the spent lead
        assert_eq!(id, 0x15);
        assert_eq!(
            client
                .inv
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&36)
                .map(|s| s.1),
            Some(1)
        );
        {
            let tethers = entity_tethers().lock().unwrap_or_else(|e| e.into_inner());
            let t = tethers.get(&mob.eid).expect("tether recorded");
            assert_eq!(t.anchor, TetherAnchor::Entity(client.eid));
            assert_eq!((t.max_length, t.elastic_start), (10.0, 6.0));
            assert!(t.break_on_stretch);
            assert_eq!(t.drop_item_on_break, Some(lead as i32));
        }

        // At 8 blocks the elastic band applies the vanilla impulse
        // copySign(d^2 * 0.4, d): one tick pulls 0.4 toward the holder.
        tether_tick();
        {
            let p = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
            assert!(
                (p.2 - 7.6).abs() < 1e-9,
                "elastic pull must move the mob 0.4 blocks, got z={}",
                p.2
            );
            assert_eq!(*mob.vel.lock().unwrap_or_else(|e| e.into_inner()), [0.0, 0.0, -0.4]);
        }
        let (id, _) = vl_read(&mut peer); // 0x70 teleport for the pull
        assert_eq!(id, 0x70);

        // Past the 10-block lead it snaps: detach on the wire and a lead drop
        // at the mob's position.
        *mob.pos.lock().unwrap_or_else(|e| e.into_inner()) = (0.0, 70.0, 11.0, 0.0, 0.0);
        tether_tick();
        assert!(entity_tethers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        assert_eq!(vl_read(&mut peer), (0x59, set_entity_link_pkt(mob.eid, -1)));
        let ds = drops().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(ds.len(), 1);
        assert_eq!((ds[0].item, ds[0].cnt), (lead, 1));
        assert_eq!((ds[0].x, ds[0].y, ds[0].z), (0.0, 70.0, 11.0));
    }

    #[test]
    fn vanilla_leash_chain_tether_clamps_at_max_length() {
        let _world_guard = world_guard();
        let mob = vl_spawn_mob(22, (20.0, 70.0, 0.0));
        let (client, _peer) = vl_client(42421, (0.0, 70.0, 0.0));
        let _cleanup = VlCleanup {
            eids: vec![mob.eid],
            client_eids: vec![client.eid],
        };
        entity_tethers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                mob.eid,
                EntityTether {
                    target_eid: mob.eid,
                    anchor: TetherAnchor::Entity(client.eid),
                    max_length: 14.0,
                    elastic_start: 6.0,
                    break_on_stretch: false,
                    drop_item_on_break: None,
                },
            );
        tether_tick();
        // 20 > 14, non-breaking: pulled onto the 14-block sphere, still held.
        let p = *mob.pos.lock().unwrap_or_else(|e| e.into_inner());
        assert!((p.0 - 14.0).abs() < 1e-9, "clamped to max_length, x={}", p.0);
        assert!(entity_tethers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&mob.eid));
        assert!(drops()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        // A second tick at exactly max_length leaves the tether intact.
        tether_tick();
        assert!(entity_tethers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&mob.eid));
    }

    #[test]
    fn vanilla_leash_fence_hitch_reanchors_and_knot_break_detaches() {
        let _world_guard = world_guard();
        let mob = vl_spawn_mob(87, (2.0, 70.0, 2.0)); // sheep
        let (client, mut peer) = vl_client(42422, (0.0, 70.0, 0.0));
        {
            let mut seen = client.seen.lock().unwrap_or_else(|e| e.into_inner());
            seen.insert(mob.eid);
        }
        let _cleanup = VlCleanup {
            eids: vec![mob.eid],
            client_eids: vec![client.eid],
        };
        attach_tether(vanilla_lead_tether(
            mob.eid,
            TetherAnchor::Entity(client.eid),
        ));
        assert_eq!(
            vl_read(&mut peer),
            (0x59, set_entity_link_pkt(mob.eid, client.eid))
        );

        // Hitch to a fence: one knot entity is created at the block and the
        // tether re-anchors to it on the wire.
        assert!(hitch_player_tethers(client.eid, DimId::Overworld, (5, 70, 5)));
        let knot = knot_eid_at(DimId::Overworld, (5, 70, 5)).expect("knot spawned");
        client
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(knot);
        assert_eq!(
            vl_read(&mut peer),
            (0x59, set_entity_link_pkt(mob.eid, knot))
        );
        {
            let tethers = entity_tethers().lock().unwrap_or_else(|e| e.into_inner());
            assert_eq!(
                tethers.get(&mob.eid).map(|t| t.anchor),
                Some(TetherAnchor::FenceKnot {
                    dim: DimId::Overworld,
                    pos: (5, 70, 5),
                    knot_eid: knot,
                })
            );
        }
        // A second hitch reuses the knot rather than spawning another.
        attach_tether(vanilla_lead_tether(
            mob.eid,
            TetherAnchor::Entity(client.eid),
        ));
        let _ = vl_read(&mut peer);
        assert!(hitch_player_tethers(client.eid, DimId::Overworld, (5, 70, 5)));
        assert_eq!(knot_eid_at(DimId::Overworld, (5, 70, 5)), Some(knot));
        let _ = vl_read(&mut peer); // re-anchor 0x59

        // Breaking the knot detaches the tether, drops the lead at the mob and
        // destroys the knot entity for its trackers.
        assert!(break_leash_knot(knot));
        assert!(entity_tethers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        assert_eq!(vl_read(&mut peer), (0x59, set_entity_link_pkt(mob.eid, -1)));
        assert_eq!(
            vl_read(&mut peer),
            (0x42, remove_entities_payload(&[knot]))
        );
        let ds = drops().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].item, name_to_item("lead").unwrap());
    }

    #[test]
    fn vanilla_leash_name_tag_renames_and_consumes() {
        let _world_guard = world_guard();
        let mob = vl_spawn_mob(77, (1.0, 70.0, 1.0)); // pig
        let (client, mut peer) = vl_client(42423, (0.0, 70.0, 0.0));
        client
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(mob.eid);
        let _cleanup = VlCleanup {
            eids: vec![mob.eid],
            client_eids: vec![client.eid],
        };
        let tag = name_to_item("name_tag").unwrap();
        // An unrenamed tag does nothing.
        client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, (tag, 1, Vec::new()));
        assert!(!leash_name_tag_interact(&client, mob.eid, 36));
        // A renamed tag applies the custom name and is consumed in survival.
        client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(36, (tag, 1, vec![(COMP_CUSTOM_NAME, nbt_string("Bessie"))]));
        assert!(leash_name_tag_interact(&client, mob.eid, 36));
        assert_eq!(
            mob_custom_names()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&mob.eid)
                .cloned(),
            Some("Bessie".to_string())
        );
        let (id, body) = vl_read(&mut peer);
        assert_eq!(id, 0x58);
        assert_eq!(body, mob_custom_name_meta_pkt(mob.eid, "Bessie"));
        // Slot 2 (Optional<TextComponent>, serializer 6) carries the name;
        // slot 3 (boolean) marks it always visible.
        let mut sl = &body[..];
        let _eid = read_varint(&mut sl).unwrap();
        assert_eq!(&sl[..4], &[2, 6, 1, 8]);
        let name_len = u16::from_be_bytes(sl[4..6].try_into().unwrap()) as usize;
        assert_eq!(&sl[6..6 + name_len], b"Bessie");
        assert_eq!(&sl[6 + name_len..], &[3, 8, 1, 0xff]);
        assert!(client
            .inv
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&36)
            .is_none());
    }

    #[test]
    fn vanilla_leash_disconnect_detaches_held_leads() {
        let _world_guard = world_guard();
        let mob = vl_spawn_mob(22, (4.0, 70.0, 4.0));
        let (client, mut peer) = vl_client(42424, (0.0, 70.0, 0.0));
        client
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(mob.eid);
        let _cleanup = VlCleanup {
            eids: vec![mob.eid],
            client_eids: vec![client.eid],
        };
        attach_tether(vanilla_lead_tether(
            mob.eid,
            TetherAnchor::Entity(client.eid),
        ));
        assert_eq!(
            vl_read(&mut peer),
            (0x59, set_entity_link_pkt(mob.eid, client.eid))
        );
        // The disconnect path calls detach_tethers_involving: held leads
        // detach with the wire form and drop at the mob.
        detach_tethers_involving(client.eid, None);
        assert!(entity_tethers()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty());
        assert_eq!(vl_read(&mut peer), (0x59, set_entity_link_pkt(mob.eid, -1)));
        let ds = drops().lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].item, name_to_item("lead").unwrap());
        assert_eq!((ds[0].x, ds[0].y, ds[0].z), (4.0, 70.0, 4.0));
    }
}
