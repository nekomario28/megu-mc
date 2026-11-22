// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Optional **Vanilla+ Tactile & Sky Effects** procedural micro-effect engine.
//!
//! This module is **opt-in and default OFF**:
//! [`TactileEffectsConfig::default`] has `preset == TactilePreset::Off`, so the
//! vanilla look and feel never change unless the user explicitly picks
//! [`TactilePreset::Subtle`] or [`TactilePreset::Fancy`]. When enabled it
//! layers six small, strictly bounded effect systems on top of the classic
//! blocky aesthetic:
//!
//! * **Water & lava physics reactions** — entry splashes whose droplet count
//!   and [`WaterRippleRing`] radius scale with body mass x impact speed,
//!   sizzling [`LavaSizzleSpark`]s and [`SteamPuff`]s on lava, crisp
//!   [`BubblePopRing`]s at the surface, and a [`WetnessTracker`] that drips
//!   [`WetDrip`]s off a body for ~5 s after it leaves water or rain.
//! * **Tree shake, footsteps & drips** — a downward-spiralling shower of
//!   sakura petals or autumn leaves when a trunk is punched
//!   ([`LeafPetal`]), surface-aware footstep/landing dust
//!   ([`FootstepPuff`], [`LandingDustRing`]) that stays quiet on plain
//!   stone/wood, and ceiling/eaves droplets with a landing ripple cue.
//! * **Cold breath & hearth embers** — soft drifting [`ColdBreathVapor`]
//!   that only fires at `temperature_c <= 2.0` or in winter (fails closed in
//!   warm biomes and vacuum), and warm rising [`HearthEmber`]s for
//!   campfires, torches and furnaces.
//! * **Combat & mining sparks** — brief crisp [`ImpactSpark`]s on ore
//!   strikes, shield blocks and crits, a subtle [`ShockRing`] on a parry,
//!   and a single ultra-thin [`ArrowTrailMote`] for fast arrows.
//! * **Night sky drama** — rare deterministic [`ShootingStar`] streaks on
//!   clear nights and softly undulating [`AuroraBand`] ribbons restricted
//!   to cold/winter clear skies with a real atmosphere.
//! * **Motion-safe camera micro-physics** — an *additional* opt-in toggle
//!   (`subtle_camera_motion`, off even under `Subtle`) producing tiny
//!   strictly bounded roll/landing-dip offsets, plus deterministic
//!   [`EnvironmentAudioHint`] occlusion/reverb/muffle parameters for the
//!   spatial audio engine.
//!
//! Everything is pure `std`, deterministic, and reads no wall clock: all
//! spread, phase and scheduling derives from caller-supplied `tick` values
//! and integer hash mixing — no RNG, no `SystemTime`, no locks. The hard
//! particle cap follows the active [`VfxHardwareTier`] (`LowSpec` 48,
//! `Balanced` 144, `High` 288) and is enforced inside the sim so frame rate
//! can never be dragged down by an event burst. No Mojang or third-party
//! geometry or texture data is referenced anywhere in this file.
#![deny(unsafe_code)]

use std::f32::consts::{PI, TAU};

use crate::vfx_engine::VfxHardwareTier;

// ---------------------------------------------------------------------------
// Shared constants (fail-closed bounds)
// ---------------------------------------------------------------------------

/// Maximum absolute world coordinate accepted anywhere in this module.
/// Matches the classic far-lands guard used by [`crate::vfx_engine`].
const MAX_COORD: f32 = 30_000_000.0;
/// Clamp for `dt_sec` so a hitch cannot explode the simulation.
const MAX_DT_SEC: f32 = 0.25;

const LOW_SPEC_TACTILE_CAP: usize = 48;
const BALANCED_TACTILE_CAP: usize = 144;
const HIGH_TACTILE_CAP: usize = 288;

/// Seconds for a soaked body to dry completely once out of water/rain.
const WETNESS_DECAY_SEC: f32 = 5.0;
/// Wetness below this produces no drips at all.
const DRIP_MIN_WETNESS: f32 = 0.15;
/// Drip interval bounds in seconds: dry-ish .. freshly soaked.
const DRIP_INTERVAL_MAX_SEC: f32 = 0.60;
const DRIP_INTERVAL_MIN_SEC: f32 = 0.22;
/// Backlog bound so a long soak cannot dump a burst of drips at once.
const MAX_PENDING_DRIPS: u8 = 3;

/// Hard motion bounds for the opt-in camera micro-physics, in radians and
/// blocks. Returned values never exceed these even for hostile inputs.
pub const CAMERA_MAX_ROLL_RAD: f32 = 0.045;
pub const CAMERA_MAX_DIP_BLOCKS: f32 = 0.14;
/// Spring stiffness/damping for the landing dip (substepped for stability).
const DIP_SPRING_K: f32 = 120.0;
const DIP_SPRING_C: f32 = 14.0;
const DIP_SUBSTEP_SEC: f32 = 0.02;

/// Vanilla day length in ticks; night is `13000..23000` of each cycle.
const DAY_LEN_TICKS: u64 = 24_000;
const NIGHT_START_TICK: u64 = 13_000;
const NIGHT_END_TICK: u64 = 23_000;
/// Minimum atmosphere for meteors to burn visibly (vacuum => no streaks).
const SHOOTING_STAR_MIN_ATMOSPHERE: f32 = 0.08;
/// Minimum atmosphere for aurora curtains.
const AURORA_MIN_ATMOSPHERE: f32 = 0.25;
/// Shooting-star slots evaluated per night, Subtle / Fancy.
const STAR_SLOTS_SUBTLE: u64 = 2;
const STAR_SLOTS_FANCY: u64 = 5;

// Deterministic hash seeds: one per feature so fields stay independent.
const SEED_SPLASH: u64 = 0x9e37_79b9_7f4a_7c15;
const SEED_LAVA: u64 = 0xc2b2_ae3d_27d4_eb4f;
const SEED_BUBBLE: u64 = 0x1656_67b1_9e37_79f9;
const SEED_PETAL: u64 = 0x85eb_ca6b_f27d_4a3d;
const SEED_STEP: u64 = 0x27d4_eb2f_1656_67c5;
const SEED_DRIP: u64 = 0x94d0_49bb_1331_11eb;
const SEED_BREATH: u64 = 0x632d_e59f_0a5a_2b71;
const SEED_EMBER: u64 = 0xd6e8_feb8_6659_fd93;
const SEED_SPARK: u64 = 0xa24b_aed8_8d8a_4b1d;
const SEED_STAR_A: u64 = 0x9e37_79b9_7f4a_7c15 ^ 0x5f35_6495;
const SEED_STAR_B: u64 = 0xc2b2_ae3d_27d4_eb4f ^ 0x27d4_eb2f;
const SEED_STAR_C: u64 = 0x1656_67b1_9e37_79f9 ^ 0x1656_67c5;
const SEED_AURORA: u64 = 0x85eb_ca6b_f27d_4a3d ^ 0x94d0_49bb;
const SEED_WETDRIP: u64 = 0x27d4_eb2f_1656_67c5 ^ 0x0a5a_2b71;

// ---------------------------------------------------------------------------
// Small numeric helpers (mirroring vfx_engine conventions)
// ---------------------------------------------------------------------------

fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

fn clamped(v: f32, lo: f32, hi: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        lo
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn valid_pos(p: &[f32; 3]) -> bool {
    p.iter().all(|c| c.is_finite() && c.abs() <= MAX_COORD)
}

fn normalized_dir(d: [f32; 3], fallback: [f32; 3]) -> [f32; 3] {
    let len_sq = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
    if len_sq.is_finite() && len_sq > 1.0e-12 {
        let inv = len_sq.sqrt().recip();
        [d[0] * inv, d[1] * inv, d[2] * inv]
    } else {
        fallback
    }
}

/// Deterministic integer hash -> `[0, 1]`. No RNG, no clock, stable across
/// rebuilds and platforms; identical mixing to `vanilla_plus_nature`.
fn hash01(x: i64, z: i64, seed: u64) -> f32 {
    let mut h = (x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (z as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
        ^ seed.wrapping_mul(0x1656_67b1_9e37_79f9);
    h ^= h >> 29;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 32;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    ((h >> 40) & 0x00ff_ffff) as f32 / 16_777_215.0
}

fn hash3(x: i64, z: i64, seed: u64) -> [f32; 3] {
    [
        hash01(x, z, seed),
        hash01(z, x, seed ^ 0x9e37_79b9),
        hash01(x.wrapping_add(z), x.wrapping_sub(z), seed ^ 0x3c79_a492),
    ]
}

// ---------------------------------------------------------------------------
// Master opt-in configuration
// ---------------------------------------------------------------------------

/// Effect density preset. `Off` is the default and is the only state in
/// which this module touches nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TactilePreset {
    /// Fully vanilla: zero particles, zero sky drama, zero camera motion.
    #[default]
    Off,
    /// Restrained counts (~55% of Fancy) for low-key immersion.
    Subtle,
    /// Full counts and the widest night-sky slot window.
    Fancy,
}

impl TactilePreset {
    /// `true` unless the preset is `Off`.
    pub const fn is_active(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Particle-count multiplier applied before the hardware cap.
    const fn intensity_scale(self) -> f32 {
        match self {
            Self::Off => 0.0,
            Self::Subtle => 0.55,
            Self::Fancy => 1.0,
        }
    }
}

/// Per-user configuration for the Vanilla+ Tactile & Sky Effects engine.
/// `Default` is fully vanilla: `preset == TactilePreset::Off` **and**
/// `subtle_camera_motion == false`.
#[derive(Clone, Debug, PartialEq)]
pub struct TactileEffectsConfig {
    /// Master opt-in preset. **`Off` by default** — the engine never
    /// changes the vanilla look unless explicitly enabled.
    pub preset: TactilePreset,
    /// Splashes, ripple rings, lava sizzle/steam, bubble pops and wet drips.
    pub water_and_lava_reactions: bool,
    /// Tree-shake leaf bursts, footstep/landing dust and ceiling drips.
    pub tree_shake_and_footsteps: bool,
    /// Cold-breath vapor and hearth embers.
    pub cold_breath_and_embers: bool,
    /// Impact sparks, shield shock rings and arrow trail motes.
    pub impact_sparks: bool,
    /// Shooting stars and polar aurora ribbons.
    pub shooting_stars_and_aurora: bool,
    /// Motion-sickness-safe camera micro-physics. **`false` by default even
    /// under `Subtle`/`Fancy`** — a separate opt-in for sensitive players.
    pub subtle_camera_motion: bool,
    /// Strength of the camera micro-physics in `0..=1`; read through
    /// [`Self::effective_camera_motion_strength`].
    pub camera_motion_strength: f32,
}

impl Default for TactileEffectsConfig {
    fn default() -> Self {
        Self {
            preset: TactilePreset::Off,
            water_and_lava_reactions: true,
            tree_shake_and_footsteps: true,
            cold_breath_and_embers: true,
            impact_sparks: true,
            shooting_stars_and_aurora: true,
            subtle_camera_motion: false,
            camera_motion_strength: 0.5,
        }
    }
}

impl TactileEffectsConfig {
    /// `camera_motion_strength` clamped to `0..=1` (non-finite -> 0).
    pub fn effective_camera_motion_strength(&self) -> f32 {
        clamped(self.camera_motion_strength, 0.0, 1.0)
    }

    fn intensity_scale(&self) -> f32 {
        self.preset.intensity_scale()
    }
}

// ---------------------------------------------------------------------------
// Particle model
// ---------------------------------------------------------------------------

/// One bounded micro-effect particle. `kind` carries the per-kind shape
/// data; `pos`/`vel`/`age_sec`/`ttl_sec` are the shared kinematics. `phase`
/// and `jitter` are per-particle hashes (`0..1`, radians) that drive
/// deterministic per-particle variation — flutter phase, ember wobble and
/// tint jitter — with no RNG state anywhere.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TactileParticle {
    pub kind: TactileParticleKind,
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    pub age_sec: f32,
    pub ttl_sec: f32,
    /// Rendered size in blocks; rings grow this toward their `max_radius`.
    pub size: f32,
    /// Flutter/wobble phase in radians.
    pub phase: f32,
    /// Secondary `0..1` hash for tint/size jitter.
    pub jitter: f32,
    /// Linear-space `0..=1` tint with alpha.
    pub rgba: [f32; 4],
}

impl TactileParticle {
    /// `true` once the particle should be dropped.
    pub fn expired(&self) -> bool {
        self.age_sec >= self.ttl_sec
    }
}

/// The tactile particle families. Each stays deliberately quiet: small
/// counts, soft colours, no screen-space noise. Ring kinds carry their
/// `max_radius` and expand deterministically toward it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TactileParticleKind {
    /// Droplet thrown upward/outward by a fluid entry splash.
    SplashDroplet,
    /// Concentric expanding ring on a water surface.
    WaterRippleRing { max_radius: f32 },
    /// Tiny crisp ring where an underwater bubble reached the surface.
    BubblePopRing { max_radius: f32 },
    /// Orange magma spark flung out of a lava surface.
    LavaSizzleSpark,
    /// Soft grey-white evaporation puff rising off lava.
    SteamPuff,
    /// Droplet falling off a still-wet body.
    WetDrip,
    /// Downward-spiralling leaf / sakura petal from a shaken canopy.
    LeafPetal,
    /// Surface dust / snow / sand / mud puff or leaf-rustle fleck.
    FootstepPuff,
    /// Flat expanding dust ring from a heavy landing.
    LandingDustRing { max_radius: f32 },
    /// Cave stalactite / rainy-eaves droplet.
    CeilingDrip,
    /// Subtle ripple cue where a [`CeilingDrip`] finished its fall.
    DripLandingRing { max_radius: f32 },
    /// Soft drifting cold-breath vapor puff.
    ColdBreathVapor,
    /// Warm rising ember from a hearth, torch or lava face.
    HearthEmber,
    /// Crisp short-lived combat/mining spark.
    ImpactSpark,
    /// Subtle expanding shock ring on a shield parry.
    ShockRing { max_radius: f32 },
    /// Ultra-thin wind-cut mote marking a fast arrow's path.
    ArrowTrailMote,
}

// ---------------------------------------------------------------------------
// Wetness tracker
// ---------------------------------------------------------------------------

/// Per-entity wetness (`0.0..=1.0`) for the [`WetDrip`] effect. Call
/// [`Self::step`] once per frame with whether the body is currently in
/// water or rain; wetness saturates quickly while wet and decays to zero
/// over ~5 s after exiting, emitting a deterministic drip pulse whose
/// interval tightens as the body gets wetter.
#[derive(Clone, Debug, PartialEq)]
pub struct WetnessTracker {
    wetness: f32,
    drip_clock_sec: f32,
    pending_drips: u8,
    position: [f32; 3],
}

impl Default for WetnessTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl WetnessTracker {
    pub fn new() -> Self {
        Self {
            wetness: 0.0,
            drip_clock_sec: 0.0,
            pending_drips: 0,
            position: [0.0; 3],
        }
    }

    /// Current wetness in `0..=1`.
    pub fn wetness(&self) -> f32 {
        self.wetness
    }

    /// `true` while wet enough to shed drips (`wetness > 0.15`).
    pub fn is_wet(&self) -> bool {
        self.wetness > DRIP_MIN_WETNESS
    }

    /// Fully dry (`wetness == 0.0` and no pending drips).
    pub fn is_dry(&self) -> bool {
        self.wetness <= 0.0 && self.pending_drips == 0
    }

    /// Feet/body anchor used when a drip particle is spawned.
    pub fn position(&self) -> [f32; 3] {
        self.position
    }

    /// Update the drip anchor; non-finite or far-lands positions are
    /// ignored so a bad entity transform can never fling a drip.
    pub fn set_position(&mut self, pos: [f32; 3]) {
        if valid_pos(&pos) {
            self.position = pos;
        }
    }

    /// Advance the tracker by `dt_sec` (clamped to `0..=0.25`). While
    /// `in_water_or_rain` the body soaks toward `1.0` and sheds nothing;
    /// once out, wetness decays linearly to exactly `0.0` over ~5 s and
    /// drip pulses accumulate behind a per-pulse interval that tightens
    /// from `0.60 s` to `0.22 s` as wetness rises.
    pub fn step(&mut self, dt_sec: f32, in_water_or_rain: bool) {
        let dt = if dt_sec.is_finite() {
            dt_sec.clamp(0.0, MAX_DT_SEC)
        } else {
            0.0
        };
        if in_water_or_rain {
            self.wetness = (self.wetness + dt * 2.5).min(1.0);
            self.drip_clock_sec = 0.0;
        } else {
            self.wetness = (self.wetness - dt / WETNESS_DECAY_SEC).max(0.0);
            if self.is_wet() {
                self.drip_clock_sec += dt;
                while self.pending_drips < MAX_PENDING_DRIPS
                    && self.drip_clock_sec >= self.drip_interval_sec()
                {
                    self.drip_clock_sec -= self.drip_interval_sec();
                    self.pending_drips += 1;
                }
            } else {
                self.drip_clock_sec = 0.0;
                if self.wetness <= 0.0 {
                    self.pending_drips = 0;
                }
            }
        }
    }

    /// Drip pulse interval for the current wetness in seconds.
    fn drip_interval_sec(&self) -> f32 {
        lerp(DRIP_INTERVAL_MAX_SEC, DRIP_INTERVAL_MIN_SEC, self.wetness)
    }

    /// Number of drip pulses ready to spawn, draining the backlog.
    pub fn take_pending_drips(&mut self) -> usize {
        let n = self.pending_drips as usize;
        self.pending_drips = 0;
        n
    }
}

// ---------------------------------------------------------------------------
// Fluid entry
// ---------------------------------------------------------------------------

/// What kind of body entered the fluid; heavier bodies make bigger splashes.
/// Ordering is mass-monotonic: `ItemDrop < Projectile < SmallMob <
/// PlayerOrMediumMob < LargeMob`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FluidEntryBody {
    /// Dropped item, fishing bobber: a tiny gentle plop.
    ItemDrop,
    /// Arrow, snowball, trident: a light plop.
    Projectile,
    /// Chicken, fish, small slime: a modest splash.
    SmallMob,
    /// Player, zombie, most mobs: a proper splash column.
    PlayerOrMediumMob,
    /// Iron golem, ravager, boat: a heavy drenching splash.
    LargeMob,
}

impl FluidEntryBody {
    /// Relative mass factor, strictly increasing across variants.
    const fn mass_factor(self) -> f32 {
        match self {
            Self::ItemDrop => 0.15,
            Self::Projectile => 0.25,
            Self::SmallMob => 0.50,
            Self::PlayerOrMediumMob => 0.85,
            Self::LargeMob => 1.45,
        }
    }

    /// Monotone rank used to keep splash counts strictly separated.
    const fn rank(self) -> u32 {
        match self {
            Self::ItemDrop => 0,
            Self::Projectile => 1,
            Self::SmallMob => 2,
            Self::PlayerOrMediumMob => 3,
            Self::LargeMob => 4,
        }
    }
}

/// One fluid-entry event: where, how fast, how heavy, and into which fluid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FluidEntryEvent {
    /// Surface contact point.
    pub pos: [f32; 3],
    /// Downward impact speed in blocks/sec (`0` = gentle touch).
    pub impact_speed: f32,
    /// Entering body class.
    pub body: FluidEntryBody,
    /// `true` for lava, `false` for water.
    pub is_lava: bool,
    /// Deterministic event tick used for all spread/phase hashing.
    pub seed_tick: u64,
}

// ---------------------------------------------------------------------------
// Tree shake, footsteps & ceiling drips
// ---------------------------------------------------------------------------

/// Canopy family selecting the petal/leaf tint for a tree-shake burst.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LeafCanopyKind {
    /// Soft pink cherry petals.
    CherrySakura,
    /// Warm amber-red maple leaves.
    AutumnMaple,
    /// Pale gold birch leaves.
    BirchGold,
    /// Mid-green oak leaves.
    OakGreen,
    /// Deep green spruce needles.
    SpruceNeedle,
    /// Caller-supplied linear `0..=1` tint (modded canopies).
    CustomTint([f32; 3]),
}

impl LeafCanopyKind {
    fn base_rgb(self) -> [f32; 3] {
        match self {
            Self::CherrySakura => [0.97, 0.70, 0.80],
            Self::AutumnMaple => [0.85, 0.36, 0.12],
            Self::BirchGold => [0.90, 0.74, 0.26],
            Self::OakGreen => [0.30, 0.55, 0.20],
            Self::SpruceNeedle => [0.16, 0.36, 0.21],
            Self::CustomTint(rgb) => rgb.map(|c| clamped(c, 0.0, 1.0)),
        }
    }
}

/// Ground surface underfoot selecting the footstep/landing dust response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroundSurfaceKind {
    /// Dry sand: pale tan puff.
    Sand,
    /// Snow layer / snow block: soft white puff.
    Snow,
    /// Gravel: grey grit flecks.
    Gravel,
    /// Mud or ankle-deep water: brown/dark splash flecks.
    MudOrShallowWater,
    /// Fallen leaf carpet: rustle of tinted flecks.
    FallenLeaves,
    /// Plain stone, planks, etc.: silent for gentle steps, a dust ring only
    /// for genuinely heavy landings.
    StoneOrWood,
}

impl GroundSurfaceKind {
    fn puff_rgb(self) -> [f32; 3] {
        match self {
            Self::Sand => [0.83, 0.76, 0.55],
            Self::Snow => [0.92, 0.95, 0.98],
            Self::Gravel => [0.55, 0.53, 0.50],
            Self::MudOrShallowWater => [0.40, 0.32, 0.24],
            Self::FallenLeaves => [0.70, 0.45, 0.20],
            Self::StoneOrWood => [0.60, 0.58, 0.55],
        }
    }
}

/// Minimum landing speed that makes plain stone/wood emit anything.
const STONE_LANDING_MIN_SPEED: f32 = 3.0;
/// Landing speed at which any surface also gets a dust ring.
const LANDING_RING_MIN_SPEED: f32 = 4.0;

// ---------------------------------------------------------------------------
// Combat & mining sparks
// ---------------------------------------------------------------------------

/// Struck material selecting the spark family for [`TactileEffectsSim::emit_impact_sparks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImpactMaterial {
    /// Shield block / armor hit: sparks plus a subtle [`ShockRing`].
    MetalArmorOrShield,
    /// Pickaxe on ore or stone: crisp grey-amber sparks.
    OreOrStone,
    /// Critical melee hit: brighter, slightly longer sparks.
    CriticalHit,
    /// Fast arrow flyby: a single ultra-thin [`ArrowTrailMote`].
    ArrowTrail,
}

// ---------------------------------------------------------------------------
// Night sky drama
// ---------------------------------------------------------------------------

/// One deterministic meteor streak across the night sky. `start_dir` /
/// `end_dir` are unit-ish sky directions; the renderer interpolates the
/// head position between them by `progress` (`0..1`) and fades by
/// `brightness` (`0..1`, already attack/decay shaped).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShootingStar {
    pub start_dir: [f32; 3],
    pub end_dir: [f32; 3],
    pub progress: f32,
    pub brightness: f32,
    pub tint: [f32; 3],
}

/// One softly undulating polar aurora ribbon. `altitude_deg` is its
/// elevation band above the horizon, `phase` a slowly advancing undulation
/// phase, `intensity` `0..1`, and the two `rgb` stops the primary
/// (emerald/teal) and secondary (violet) curtain colours.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AuroraBand {
    pub altitude_deg: f32,
    pub phase: f32,
    pub intensity: f32,
    pub primary_rgb: [f32; 3],
    pub secondary_rgb: [f32; 3],
}

/// Night-sky output for one tick: zero or more shooting stars plus the
/// active aurora ribbons. Empty on `Off` preset, daytime, rain, or vacuum.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NightSkyDrama {
    pub shooting_stars: Vec<ShootingStar>,
    pub aurora_bands: Vec<AuroraBand>,
}

impl NightSkyDrama {
    /// `true` when nothing is active this tick.
    pub fn is_empty(&self) -> bool {
        self.shooting_stars.is_empty() && self.aurora_bands.is_empty()
    }
}

/// Evaluates the deterministic night-sky layer for this tick.
///
/// * **Shooting stars**: rare streaks, only inside the vanilla night window
///   (tick `13000..23000`), only under a clear sky, and only with some
///   atmosphere (`> 0.08`) to burn in. Each night gets a fixed set of hash-
///   scheduled slots, so replaying the same `world_time_ticks` always
///   reproduces the same streaks.
/// * **Polar aurora**: `is_cold_or_winter` clear nights with
///   `atmosphere_density > 0.25` get 2-3 softly undulating ribbons; deserts,
///   the nether/end, daytime and rain get exactly nothing.
///
/// Returns an empty [`NightSkyDrama`] whenever the preset is `Off` or the
/// `shooting_stars_and_aurora` toggle is off.
pub fn evaluate_night_sky_drama(
    world_time_ticks: u64,
    is_clear_sky: bool,
    is_cold_or_winter: bool,
    atmosphere_density: f32,
    config: &TactileEffectsConfig,
) -> NightSkyDrama {
    let mut drama = NightSkyDrama::default();
    if !config.preset.is_active()
        || !config.shooting_stars_and_aurora
        || !is_clear_sky
        || !atmosphere_density.is_finite()
    {
        return drama;
    }

    let tod = world_time_ticks % DAY_LEN_TICKS;
    if !(NIGHT_START_TICK..NIGHT_END_TICK).contains(&tod) {
        return drama;
    }
    let night_index = world_time_ticks / DAY_LEN_TICKS;
    let density = clamped(atmosphere_density, 0.0, 1.0);

    // Shooting stars: hash-scheduled slots within this night.
    if density > SHOOTING_STAR_MIN_ATMOSPHERE {
        let slots = match config.preset {
            TactilePreset::Off => 0,
            TactilePreset::Subtle => STAR_SLOTS_SUBTLE,
            TactilePreset::Fancy => STAR_SLOTS_FANCY,
        };
        let window = NIGHT_END_TICK - NIGHT_START_TICK - 120;
        for slot in 0..slots {
            let h1 = hash01(night_index as i64, slot as i64 * 7 + 1, SEED_STAR_A);
            let h2 = hash01(slot as i64 * 13 + 3, night_index as i64, SEED_STAR_B);
            let h3 = hash01(
                night_index as i64 + slot as i64,
                slot as i64 + 77,
                SEED_STAR_C,
            );
            let t_start = NIGHT_START_TICK + (h1 * window as f32) as u64;
            let duration = 18 + (h2 * 22.0) as u64;
            if tod < t_start || tod >= t_start + duration {
                continue;
            }
            let progress = (tod - t_start) as f32 / duration as f32;
            let brightness = (PI * progress).sin() * (0.55 + 0.45 * h2);

            let az = h1 * TAU;
            let el = 0.55 + 0.55 * h2;
            let az2 = az + 0.30 + 0.35 * h3;
            let el2 = (el - 0.10 - 0.18 * h3).max(0.15);
            let dir = |azimuth: f32, elev: f32| {
                [
                    elev.cos() * azimuth.cos(),
                    elev.sin(),
                    elev.cos() * azimuth.sin(),
                ]
            };
            let tint = if h3 < 0.3 {
                [1.0, 0.88, 0.72]
            } else {
                [0.88, 0.94, 1.0]
            };
            drama.shooting_stars.push(ShootingStar {
                start_dir: dir(az, el),
                end_dir: dir(az2, el2),
                progress,
                brightness,
                tint,
            });
        }
    }

    // Polar aurora: cold/winter clear nights with a real atmosphere only.
    if is_cold_or_winter && density > AURORA_MIN_ATMOSPHERE {
        let bands = match config.preset {
            TactilePreset::Off => 0,
            TactilePreset::Subtle => 2,
            TactilePreset::Fancy => 3,
        };
        let preset_gain = match config.preset {
            TactilePreset::Off => 0.0,
            TactilePreset::Subtle => 0.7,
            TactilePreset::Fancy => 1.0,
        };
        for band in 0..bands {
            let h1 = hash01(
                band as i64 * 5 + 1,
                night_index as i64 + band as i64,
                SEED_AURORA,
            );
            let h2 = hash01(
                night_index as i64 - band as i64,
                band as i64 * 11 + 3,
                SEED_AURORA,
            );
            let altitude_deg = 16.0 + band as f32 * 11.0 + 7.0 * h1;
            let phase = h2 * TAU + world_time_ticks as f32 * 0.013;
            let intensity = (0.40 + 0.40 * h1) * density * preset_gain;
            let primary_rgb = [
                0.15 + 0.10 * h2,
                0.85 - 0.10 * band as f32,
                0.50 + 0.15 * h1,
            ];
            let secondary_rgb = [0.45 + 0.10 * h1, 0.30 + 0.08 * h2, 0.90];
            drama.aurora_bands.push(AuroraBand {
                altitude_deg,
                phase,
                intensity,
                primary_rgb,
                secondary_rgb,
            });
        }
    }

    drama
}

// ---------------------------------------------------------------------------
// Opt-in camera micro-physics
// ---------------------------------------------------------------------------

/// Output of one [`CameraMicroMotionState::step`]: a tiny camera roll and a
/// downward landing dip, both strictly bounded and always finite.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraMicroOffset {
    /// Camera roll in radians, `|roll| <= CAMERA_MAX_ROLL_RAD`.
    pub roll_rad: f32,
    /// Downward camera dip in blocks, `0 <= dip <= CAMERA_MAX_DIP_BLOCKS`.
    pub landing_dip_blocks: f32,
}

/// Smoothed state for the opt-in camera micro-physics. While disabled
/// (`preset == Off` or `subtle_camera_motion == false`) every step returns
/// an exact zero offset and the internal state is held at rest, so toggling
/// the feature on later never produces a jump.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraMicroMotionState {
    roll_rad: f32,
    dip_blocks: f32,
    dip_vel: f32,
}

impl CameraMicroMotionState {
    /// Advance the micro-physics one frame. All inputs are sanitized
    /// (non-finite -> 0, clamped), `dt_sec` is clamped to `0..=0.25`, and
    /// the returned offset satisfies `|roll_rad| <= 0.045` and
    /// `landing_dip_blocks <= 0.14` unconditionally.
    ///
    /// * `strafe_vel`: sideways velocity in blocks/sec -> gentle lean.
    /// * `vertical_vel`: up/down velocity -> a whisper of counter-roll.
    /// * `just_landed_speed`: impact speed of this frame's landing (`0`
    ///   while airborne) -> impulse into the dip spring.
    /// * `elytra_roll_input`: pilot roll input in `-1..=1` while gliding.
    pub fn step(
        &mut self,
        strafe_vel: f32,
        vertical_vel: f32,
        just_landed_speed: f32,
        elytra_roll_input: f32,
        dt_sec: f32,
        config: &TactileEffectsConfig,
    ) -> CameraMicroOffset {
        let enabled = config.preset.is_active() && config.subtle_camera_motion;
        let strength = config.effective_camera_motion_strength();
        if !enabled || strength <= 0.0 {
            self.roll_rad = 0.0;
            self.dip_blocks = 0.0;
            self.dip_vel = 0.0;
            return CameraMicroOffset::default();
        }

        let dt = if dt_sec.is_finite() {
            dt_sec.clamp(0.0, MAX_DT_SEC)
        } else {
            0.0
        };
        let strafe = finite_or(strafe_vel, 0.0).clamp(-40.0, 40.0);
        let vvel = finite_or(vertical_vel, 0.0).clamp(-60.0, 60.0);
        let landed = finite_or(just_landed_speed, 0.0).clamp(0.0, 30.0);
        let roll_in = finite_or(elytra_roll_input, 0.0).clamp(-1.0, 1.0);

        // Roll target: strafe lean + pilot roll + faint counter-roll when
        // moving vertically. `strafe / 6` is squashed through tanh so even a
        // 40 blocks/sec dash stays gentle.
        let target_roll = (0.020 * (strafe / 6.0).tanh() + 0.030 * roll_in
            - 0.0006 * vvel.clamp(-8.0, 8.0))
            * strength;
        let target_roll = target_roll.clamp(-CAMERA_MAX_ROLL_RAD, CAMERA_MAX_ROLL_RAD);
        let approach = 1.0 - (-10.0 * dt).exp();
        self.roll_rad += (target_roll - self.roll_rad) * approach;
        self.roll_rad =
            finite_or(self.roll_rad, 0.0).clamp(-CAMERA_MAX_ROLL_RAD, CAMERA_MAX_ROLL_RAD);

        // Landing dip: critically-damped-ish spring kicked by the landing
        // impulse. Substepped so `dt == 0.25` cannot go unstable.
        if landed > 0.5 {
            self.dip_vel += 0.08 * landed * strength;
        }
        if dt > 0.0 {
            let steps = ((dt / DIP_SUBSTEP_SEC).ceil() as u32).max(1);
            let h = dt / steps as f32;
            for _ in 0..steps {
                let acc = -DIP_SPRING_K * self.dip_blocks - DIP_SPRING_C * self.dip_vel;
                self.dip_vel += acc * h;
                self.dip_blocks += self.dip_vel * h;
            }
            self.dip_vel = finite_or(self.dip_vel, 0.0).clamp(-8.0, 8.0);
            self.dip_blocks = finite_or(self.dip_blocks, 0.0)
                .clamp(-CAMERA_MAX_DIP_BLOCKS, CAMERA_MAX_DIP_BLOCKS);
        }

        CameraMicroOffset {
            roll_rad: self.roll_rad,
            landing_dip_blocks: self.dip_blocks.clamp(0.0, CAMERA_MAX_DIP_BLOCKS),
        }
    }
}

// ---------------------------------------------------------------------------
// Environment audio hint
// ---------------------------------------------------------------------------

/// Deterministic occlusion/reverb/muffle parameters for the spatial audio
/// engine. All fields are `0..1` mix weights; `0` means "leave the vanilla
/// sound alone".
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EnvironmentAudioHint {
    /// Low-pass muffle while the listener is underwater (0 = dry).
    pub underwater_lowpass: f32,
    /// Cave reverb send from darkness x enclosure (0 = open air).
    pub cave_reverb_mix: f32,
    /// How much rain-on-eaves is occluded by overhead cover (0 = exposed).
    pub rain_eaves_occlusion: f32,
}

/// Computes the environment audio hint for the listener this frame.
///
/// * `is_submerged`: listener camera under a fluid surface.
/// * `sky_light_0_to_15`: vanilla sky light at the ear position.
/// * `enclosed_wall_ratio`: `0..1` fraction of surrounding directions
///   blocked by solid geometry (0 = open field, 1 = sealed cave).
/// * `is_raining`: precipitation active overhead.
pub fn compute_environment_audio_hint(
    is_submerged: bool,
    sky_light_0_to_15: u8,
    enclosed_wall_ratio: f32,
    is_raining: bool,
) -> EnvironmentAudioHint {
    let enclosed = clamped(enclosed_wall_ratio, 0.0, 1.0);
    let darkness = 1.0 - f32::from(sky_light_0_to_15.min(15)) / 15.0;
    EnvironmentAudioHint {
        underwater_lowpass: if is_submerged { 0.8 } else { 0.0 },
        cave_reverb_mix: (enclosed * darkness * 0.9).clamp(0.0, 0.85),
        rain_eaves_occlusion: if is_raining {
            (enclosed * (0.55 + 0.35 * darkness)).clamp(0.0, 0.9)
        } else {
            0.0
        },
    }
}

// ---------------------------------------------------------------------------
// Simulation
// ---------------------------------------------------------------------------

/// Retained tactile-effect state: a strictly capped particle `Vec`, the
/// [`WetnessTracker`] for the followed entity, and the
/// [`CameraMicroMotionState`]. Construct via [`Self::new`]; every `emit_*`
/// method returns how many particles were actually spawned after the
/// hardware-tier cap.
#[derive(Clone, Debug)]
pub struct TactileEffectsSim {
    config: TactileEffectsConfig,
    tier: VfxHardwareTier,
    particles: Vec<TactileParticle>,
    wetness: WetnessTracker,
    camera: CameraMicroMotionState,
    tick_index: u64,
}

impl Default for TactileEffectsSim {
    fn default() -> Self {
        Self::new(TactileEffectsConfig::default(), VfxHardwareTier::Balanced)
    }
}

impl TactileEffectsSim {
    pub fn new(config: TactileEffectsConfig, tier: VfxHardwareTier) -> Self {
        Self {
            config,
            tier,
            particles: Vec::new(),
            wetness: WetnessTracker::new(),
            camera: CameraMicroMotionState::default(),
            tick_index: 0,
        }
    }

    /// Hard cap on simultaneously live tactile particles for `tier`:
    /// `LowSpec` 48, `Balanced` 144, `High` 288.
    pub const fn particle_cap_for_tier(tier: VfxHardwareTier) -> usize {
        match tier {
            VfxHardwareTier::LowSpec => LOW_SPEC_TACTILE_CAP,
            VfxHardwareTier::Balanced => BALANCED_TACTILE_CAP,
            VfxHardwareTier::High => HIGH_TACTILE_CAP,
        }
    }

    pub const fn tier(&self) -> VfxHardwareTier {
        self.tier
    }

    /// Re-tiers the budget; live particles beyond the new cap are trimmed
    /// (oldest first) so the invariant holds immediately.
    pub fn set_tier(&mut self, tier: VfxHardwareTier) {
        self.tier = tier;
        let cap = self.particle_cap();
        if self.particles.len() > cap {
            let excess = self.particles.len() - cap;
            self.particles.drain(..excess);
        }
    }

    /// Effective particle cap for the current tier.
    pub const fn particle_cap(&self) -> usize {
        Self::particle_cap_for_tier(self.tier)
    }

    pub fn config(&self) -> &TactileEffectsConfig {
        &self.config
    }

    /// Swap the configuration. Existing particles keep ageing out
    /// naturally even if the preset drops to `Off`.
    pub fn set_config(&mut self, config: TactileEffectsConfig) {
        self.config = config;
    }

    pub fn particles(&self) -> &[TactileParticle] {
        &self.particles
    }

    pub fn len(&self) -> usize {
        self.particles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.particles.is_empty()
    }

    pub fn wetness(&self) -> &WetnessTracker {
        &self.wetness
    }

    pub fn wetness_mut(&mut self) -> &mut WetnessTracker {
        &mut self.wetness
    }

    pub fn camera(&self) -> &CameraMicroMotionState {
        &self.camera
    }

    pub fn camera_mut(&mut self) -> &mut CameraMicroMotionState {
        &mut self.camera
    }

    /// `true` while the preset allows new spawns (`Subtle` or `Fancy`).
    fn effects_on(&self) -> bool {
        self.config.preset.is_active()
    }

    fn try_spawn(&mut self, particle: TactileParticle) -> bool {
        if self.particles.len() >= self.particle_cap() {
            return false;
        }
        self.particles.push(particle);
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_particle(
        &mut self,
        kind: TactileParticleKind,
        pos: [f32; 3],
        vel: [f32; 3],
        ttl_sec: f32,
        size: f32,
        phase: f32,
        jitter: f32,
        rgba: [f32; 4],
    ) -> bool {
        self.try_spawn(TactileParticle {
            kind,
            pos,
            vel,
            age_sec: 0.0,
            ttl_sec,
            size,
            phase,
            jitter,
            rgba,
        })
    }

    // ----- 1. Water & lava physics reactions --------------------------------

    /// One fluid-entry event. Water: a mass- and speed-scaled droplet plume
    /// plus concentric [`WaterRippleRing`]s (tiny plop for an item, rich
    /// column + wide ring for a diving golem). Lava: [`LavaSizzleSpark`]s
    /// and rising [`SteamPuff`]s. Returns particles spawned (0 when the
    /// preset or `water_and_lava_reactions` is off, or the event is invalid).
    pub fn emit_fluid_entry(&mut self, event: &FluidEntryEvent) -> usize {
        if !self.effects_on()
            || !self.config.water_and_lava_reactions
            || !valid_pos(&event.pos)
            || !event.impact_speed.is_finite()
        {
            return 0;
        }
        let speed_norm = clamped(event.impact_speed, 0.0, 12.0) / 12.0;
        let scale = self.config.intensity_scale();
        let mass = event.body.mass_factor();
        let tick = event.seed_tick as i64;
        let mut spawned = 0usize;

        if event.is_lava {
            let sparks = 1
                + (16.0 * mass * speed_norm * scale + event.body.rank() as f32 * 0.9).round()
                    as usize;
            for i in 0..sparks {
                let h = hash3(tick, i as i64, SEED_LAVA);
                let angle = h[0] * TAU;
                let vel = [
                    angle.cos() * (0.6 + 1.4 * h[1]),
                    2.0 + 3.0 * h[2] * (0.4 + speed_norm),
                    angle.sin() * (0.6 + 1.4 * h[1]),
                ];
                let rgba = [1.0, 0.35 + 0.25 * h[1], 0.08, 0.9];
                if self.spawn_particle(
                    TactileParticleKind::LavaSizzleSpark,
                    event.pos,
                    vel,
                    0.4 + 0.4 * h[0],
                    0.05 + 0.04 * h[2],
                    h[0] * TAU,
                    h[1],
                    rgba,
                ) {
                    spawned += 1;
                }
            }
            let puffs = 1 + (6.0 * mass * speed_norm * scale).round() as usize;
            for i in 0..puffs {
                let h = hash3(tick + 41, i as i64, SEED_LAVA ^ 0x77aa);
                let pos = [
                    event.pos[0] + (h[0] - 0.5) * 0.5,
                    event.pos[1] + 0.05,
                    event.pos[2] + (h[2] - 0.5) * 0.5,
                ];
                if self.spawn_particle(
                    TactileParticleKind::SteamPuff,
                    pos,
                    [0.0, 0.4 + 0.4 * h[1], 0.0],
                    0.8 + 0.6 * h[2],
                    0.15 + 0.15 * h[0],
                    h[0] * TAU,
                    h[1],
                    [0.85, 0.85, 0.88, 0.35],
                ) {
                    spawned += 1;
                }
            }
            return spawned;
        }

        // Water: droplet count strictly increases with body rank and speed.
        let droplets =
            1 + (26.0 * mass * speed_norm * scale + event.body.rank() as f32).round() as usize;
        for i in 0..droplets {
            let h = hash3(tick, i as i64, SEED_SPLASH);
            let angle = h[0] * TAU;
            let spread = 0.4 + 1.6 * speed_norm;
            let vel = [
                angle.cos() * spread * (0.4 + h[1]),
                (1.5 + 4.5 * speed_norm) * (0.5 + 0.8 * h[2]),
                angle.sin() * spread * (0.4 + h[1]),
            ];
            let pos = [
                event.pos[0] + (h[1] - 0.5) * 0.3,
                event.pos[1],
                event.pos[2] + (h[0] - 0.5) * 0.3,
            ];
            let shade = 0.75 + 0.20 * h[2];
            if self.spawn_particle(
                TactileParticleKind::SplashDroplet,
                pos,
                vel,
                0.5 + 0.5 * h[0],
                0.05 + 0.05 * h[1],
                h[0] * TAU,
                h[1],
                [0.45 * shade, 0.65 * shade, shade, 0.85],
            ) {
                spawned += 1;
            }
        }

        // Concentric ripple rings: heavy fast bodies get a second ring.
        let rings = 1 + usize::from(mass * speed_norm > 0.8);
        for r in 0..rings {
            let h = hash01(tick, r as i64 + 3, SEED_SPLASH ^ 0x55aa);
            let max_radius = (0.35 + 2.4 * mass * speed_norm) * (1.0 + 0.8 * r as f32) + 0.1 * h;
            if self.spawn_particle(
                TactileParticleKind::WaterRippleRing { max_radius },
                event.pos,
                [0.0; 3],
                0.9 + 0.4 * h,
                0.05,
                0.0,
                h,
                [0.70, 0.85, 0.95, 0.5],
            ) {
                spawned += 1;
            }
        }
        spawned
    }

    /// A tiny crisp surface ring plus micro-droplets where an underwater
    /// bubble reached the surface.
    pub fn emit_bubble_pop(&mut self, pos: [f32; 3], tick: u64) -> usize {
        if !self.effects_on() || !self.config.water_and_lava_reactions || !valid_pos(&pos) {
            return 0;
        }
        let mut spawned = 0usize;
        let h0 = hash01(tick as i64, 0, SEED_BUBBLE);
        if self.spawn_particle(
            TactileParticleKind::BubblePopRing {
                max_radius: 0.12 + 0.10 * h0,
            },
            pos,
            [0.0; 3],
            0.35,
            0.02,
            0.0,
            h0,
            [0.80, 0.90, 0.98, 0.6],
        ) {
            spawned += 1;
        }
        for i in 0..3 {
            let h = hash3(tick as i64, i as i64 + 1, SEED_BUBBLE);
            let angle = h[0] * TAU;
            if self.spawn_particle(
                TactileParticleKind::SplashDroplet,
                pos,
                [angle.cos() * 0.35, 0.6 + 0.8 * h[1], angle.sin() * 0.35],
                0.35 + 0.2 * h[2],
                0.03,
                h[0] * TAU,
                h[1],
                [0.55, 0.72, 0.92, 0.8],
            ) {
                spawned += 1;
            }
        }
        spawned
    }

    // ----- 2. Tree shake, footsteps & ceiling drips -------------------------

    /// Downward-spiralling petal/leaf shower when a trunk is punched,
    /// chopped or gust-hit. `intensity` in `0..=1` scales the petal count;
    /// the hardware cap still bounds the total.
    pub fn emit_tree_shake_burst(
        &mut self,
        canopy_pos: [f32; 3],
        kind: LeafCanopyKind,
        intensity: f32,
        tick: u64,
    ) -> usize {
        if !self.effects_on() || !self.config.tree_shake_and_footsteps || !valid_pos(&canopy_pos) {
            return 0;
        }
        let intensity = clamped(intensity, 0.0, 1.0);
        let scale = self.config.intensity_scale();
        let count = (2.0 + 16.0 * intensity * scale).round() as usize;
        let base = kind.base_rgb();
        let mut spawned = 0usize;
        for i in 0..count {
            let h = hash3(tick as i64, i as i64, SEED_PETAL);
            let pos = [
                canopy_pos[0] + (h[0] - 0.5) * 1.6,
                canopy_pos[1] + (h[1] - 0.5) * 0.8,
                canopy_pos[2] + (h[2] - 0.5) * 1.6,
            ];
            let jitter = h[1] * 0.12 - 0.06;
            let rgba = [
                (base[0] + jitter).clamp(0.0, 1.0),
                (base[1] + jitter).clamp(0.0, 1.0),
                (base[2] + jitter).clamp(0.0, 1.0),
                0.9,
            ];
            if self.spawn_particle(
                TactileParticleKind::LeafPetal,
                pos,
                [0.0, -0.55, 0.0],
                3.0 + 2.5 * h[2],
                0.07 + 0.05 * h[0],
                h[0] * TAU,
                h[1],
                rgba,
            ) {
                spawned += 1;
            }
        }
        spawned
    }

    /// Footstep or landing response for `surface`. Gentle steps on plain
    /// stone/wood emit nothing; heavy landings anywhere get a
    /// [`LandingDustRing`] scaled by `landing_speed`.
    pub fn emit_footstep_or_landing(
        &mut self,
        pos: [f32; 3],
        surface: GroundSurfaceKind,
        landing_speed: f32,
        tick: u64,
    ) -> usize {
        if !self.effects_on() || !self.config.tree_shake_and_footsteps || !valid_pos(&pos) {
            return 0;
        }
        let speed = clamped(landing_speed, 0.0, 40.0);
        let speed_norm = speed / 12.0;
        let scale = self.config.intensity_scale();
        let mut spawned = 0usize;

        if matches!(surface, GroundSurfaceKind::StoneOrWood) {
            if speed < STONE_LANDING_MIN_SPEED {
                return 0;
            }
        } else {
            let puffs = 1 + (5.0 * speed_norm * scale).round() as usize;
            let rgb = surface.puff_rgb();
            for i in 0..puffs {
                let h = hash3(tick as i64, i as i64, SEED_STEP);
                let angle = h[0] * TAU;
                let pos_jitter = 0.15 + 0.25 * speed_norm;
                let pos = [
                    pos[0] + (h[0] - 0.5) * pos_jitter * 2.0,
                    pos[1] + 0.03,
                    pos[2] + (h[1] - 0.5) * pos_jitter * 2.0,
                ];
                if self.spawn_particle(
                    TactileParticleKind::FootstepPuff,
                    pos,
                    [
                        angle.cos() * 0.25 * (0.5 + h[1]),
                        0.15 + 0.25 * h[2] + 0.2 * speed_norm,
                        angle.sin() * 0.25 * (0.5 + h[1]),
                    ],
                    0.45 + 0.35 * h[2],
                    0.05 + 0.05 * h[0] + 0.05 * speed_norm,
                    h[0] * TAU,
                    h[1],
                    [rgb[0], rgb[1], rgb[2], 0.55],
                ) {
                    spawned += 1;
                }
            }
        }

        if speed >= LANDING_RING_MIN_SPEED {
            let h = hash01(tick as i64, 7, SEED_STEP ^ 0x33cc);
            let max_radius = 0.30 + 0.12 * speed_norm.max(0.0) + 0.15 * h;
            if self.spawn_particle(
                TactileParticleKind::LandingDustRing { max_radius },
                [pos[0], pos[1] + 0.02, pos[2]],
                [0.0; 3],
                0.55 + 0.25 * h,
                0.06,
                0.0,
                h,
                [
                    surface.puff_rgb()[0],
                    surface.puff_rgb()[1],
                    surface.puff_rgb()[2],
                    0.45,
                ],
            ) {
                spawned += 1;
            }
        }
        spawned
    }

    /// Cave stalactite / rainy-eaves droplet. The droplet falls under
    /// gravity and, on expiry, leaves a subtle [`DripLandingRing`] cue at
    /// its final position.
    pub fn emit_ceiling_or_eaves_drip(&mut self, pos: [f32; 3], is_lava: bool, tick: u64) -> usize {
        if !self.effects_on() || !self.config.tree_shake_and_footsteps || !valid_pos(&pos) {
            return 0;
        }
        let h = hash01(tick as i64, 0, SEED_DRIP);
        let rgba = if is_lava {
            [1.0, 0.45, 0.15, 0.9]
        } else {
            [0.55, 0.70, 0.95, 0.8]
        };
        usize::from(self.spawn_particle(
            TactileParticleKind::CeilingDrip,
            pos,
            [0.0, -0.2, 0.0],
            0.55 + 0.25 * h,
            0.04,
            h * TAU,
            h,
            rgba,
        ))
    }

    // ----- 3. Cold breath & hearth embers ------------------------------------

    /// Soft drifting cold-breath vapor. Fails closed: emits **nothing**
    /// unless `temperature_c` is finite and `temperature_c <= 2.0` or
    /// `is_winter` — warm/tropical biomes and vacuum space stay silent.
    pub fn emit_cold_breath(
        &mut self,
        mouth_pos: [f32; 3],
        look_dir: [f32; 3],
        temperature_c: f32,
        is_winter: bool,
        tick: u64,
    ) -> usize {
        if !self.effects_on()
            || !self.config.cold_breath_and_embers
            || !valid_pos(&mouth_pos)
            || !temperature_c.is_finite()
            || !(temperature_c <= 2.0 || is_winter)
        {
            return 0;
        }
        let dir = normalized_dir(look_dir, [0.0, 0.0, 1.0]);
        // Colder air -> slightly more visible breath.
        let chill = if is_winter && temperature_c > 2.0 {
            0.6
        } else {
            clamped((2.0 - temperature_c) / 25.0, 0.2, 1.0)
        };
        let scale = self.config.intensity_scale();
        let count = 1 + usize::from(chill * scale > 0.45);
        let mut spawned = 0usize;
        for i in 0..count {
            let h = hash3(tick as i64, i as i64, SEED_BREATH);
            let start = [
                mouth_pos[0] + dir[0] * 0.25,
                mouth_pos[1] + dir[1] * 0.25 - 0.05,
                mouth_pos[2] + dir[2] * 0.25,
            ];
            let vel = [
                dir[0] * 0.4 + (h[0] - 0.5) * 0.08,
                dir[1] * 0.4 + 0.12 + (h[1] - 0.5) * 0.06,
                dir[2] * 0.4 + (h[2] - 0.5) * 0.08,
            ];
            if self.spawn_particle(
                TactileParticleKind::ColdBreathVapor,
                start,
                vel,
                1.1 + 0.5 * h[0],
                0.08 + 0.05 * chill,
                h[0] * TAU,
                h[1],
                [0.92, 0.95, 1.0, 0.28 + 0.15 * chill],
            ) {
                spawned += 1;
            }
        }
        spawned
    }

    /// Warm orange-gold embers drifting up from campfires, torches,
    /// furnaces and lava faces. `intensity` in `0..=1` scales the count.
    pub fn emit_hearth_ember(&mut self, source_pos: [f32; 3], intensity: f32, tick: u64) -> usize {
        if !self.effects_on() || !self.config.cold_breath_and_embers || !valid_pos(&source_pos) {
            return 0;
        }
        let intensity = clamped(intensity, 0.0, 1.0);
        let scale = self.config.intensity_scale();
        let count = 1 + (2.0 * intensity * scale).round() as usize;
        let mut spawned = 0usize;
        for i in 0..count {
            let h = hash3(tick as i64, i as i64, SEED_EMBER);
            let pos = [
                source_pos[0] + (h[0] - 0.5) * 0.4,
                source_pos[1] + 0.1 * h[1],
                source_pos[2] + (h[2] - 0.5) * 0.4,
            ];
            let warmth = 0.55 + 0.30 * h[1];
            if self.spawn_particle(
                TactileParticleKind::HearthEmber,
                pos,
                [(h[0] - 0.5) * 0.15, 0.45 + 0.55 * h[2], (h[2] - 0.5) * 0.15],
                1.6 + 1.4 * h[0],
                0.035 + 0.03 * h[1],
                h[0] * TAU,
                h[1],
                [1.0, warmth, 0.15 + 0.10 * h[2], 0.85],
            ) {
                spawned += 1;
            }
        }
        spawned
    }

    // ----- 4. Combat & mining sparks ----------------------------------------

    /// Brief crisp sparks on an ore strike / shield block / armor hit.
    /// `MetalArmorOrShield` also gets a subtle [`ShockRing`]; `ArrowTrail`
    /// emits exactly one ultra-thin mote (bounded by construction).
    pub fn emit_impact_sparks(
        &mut self,
        pos: [f32; 3],
        normal: [f32; 3],
        material: ImpactMaterial,
        tick: u64,
    ) -> usize {
        if !self.effects_on() || !self.config.impact_sparks || !valid_pos(&pos) {
            return 0;
        }
        let n = normalized_dir(normal, [0.0, 1.0, 0.0]);
        let scale = self.config.intensity_scale();
        let mut spawned = 0usize;

        if matches!(material, ImpactMaterial::ArrowTrail) {
            let h = hash01(tick as i64, 0, SEED_SPARK);
            return usize::from(self.spawn_particle(
                TactileParticleKind::ArrowTrailMote,
                pos,
                [0.0; 3],
                0.40,
                0.015,
                0.0,
                h,
                [0.90, 0.93, 0.98, 0.5],
            ));
        }

        let (sparks, ttl_base, rgb, speed) = match material {
            ImpactMaterial::OreOrStone => (
                (4.0 * scale + 1.0).round() as usize,
                0.30,
                [0.95, 0.75, 0.40],
                2.2,
            ),
            ImpactMaterial::MetalArmorOrShield => (
                (5.0 * scale + 1.0).round() as usize,
                0.28,
                [0.95, 0.90, 0.75],
                2.6,
            ),
            ImpactMaterial::CriticalHit => (
                (8.0 * scale + 1.0).round() as usize,
                0.42,
                [1.0, 0.85, 0.30],
                3.0,
            ),
            ImpactMaterial::ArrowTrail => unreachable!(),
        };
        for i in 0..sparks {
            let h = hash3(tick as i64, i as i64, SEED_SPARK);
            // Bias the spray along the surface normal with hashed jitter.
            let jx = (h[0] - 0.5) * 1.6;
            let jy = (h[1] - 0.5) * 1.6;
            let jz = (h[2] - 0.5) * 1.6;
            let vel = [
                n[0] * speed * (0.5 + h[1]) + jx,
                n[1] * speed * (0.5 + h[2]) + jy + 0.8,
                n[2] * speed * (0.5 + h[0]) + jz,
            ];
            if self.spawn_particle(
                TactileParticleKind::ImpactSpark,
                pos,
                vel,
                ttl_base * (0.7 + 0.6 * h[2]),
                0.03 + 0.02 * h[0],
                h[0] * TAU,
                h[1],
                [rgb[0], rgb[1], rgb[2], 0.9],
            ) {
                spawned += 1;
            }
        }
        if matches!(material, ImpactMaterial::MetalArmorOrShield) {
            let h = hash01(tick as i64, 9, SEED_SPARK ^ 0x66dd);
            if self.spawn_particle(
                TactileParticleKind::ShockRing {
                    max_radius: 0.55 + 0.35 * h,
                },
                pos,
                [0.0; 3],
                0.30 + 0.15 * h,
                0.05,
                0.0,
                h,
                [0.85, 0.90, 1.0, 0.5],
            ) {
                spawned += 1;
            }
        }
        spawned
    }

    // ----- Simulation step ---------------------------------------------------

    /// Convenience wrapper: update the wetness tracker then advance the
    /// sim. Equivalent to `sim.wetness_mut().step(dt, wet); sim.step(dt)`.
    pub fn step_with_wetness(&mut self, dt_sec: f32, in_water_or_rain: bool) {
        self.wetness.step(dt_sec, in_water_or_rain);
        self.step(dt_sec);
    }

    /// Advances the tactile simulation by `dt_sec` (clamped to
    /// `0..=0.25`, non-finite treated as `0`). Physics are deterministic:
    /// splash/ember gravity, vapor/steam buoyancy, petal spiral flutter,
    /// ring expansion derived purely from `age/ttl`. Finished particles
    /// expire; an expiring [`CeilingDrip`] leaves a [`DripLandingRing`] at
    /// its final position; pending [`WetDrip`] pulses from the
    /// [`WetnessTracker`] spawn at the tracked position. The hardware-tier
    /// cap is enforced on every spawn path.
    pub fn step(&mut self, dt_sec: f32) {
        let dt = if dt_sec.is_finite() {
            dt_sec.clamp(0.0, MAX_DT_SEC)
        } else {
            0.0
        };
        self.tick_index = self.tick_index.wrapping_add(1);

        let mut landed_drips: Vec<[f32; 3]> = Vec::new();
        for p in &mut self.particles {
            p.age_sec += dt;
            let t = if p.ttl_sec > 0.0 {
                clamped(p.age_sec / p.ttl_sec, 0.0, 1.0)
            } else {
                1.0
            };
            match &mut p.kind {
                TactileParticleKind::SplashDroplet
                | TactileParticleKind::WetDrip
                | TactileParticleKind::CeilingDrip => {
                    p.vel[1] -= 14.0 * dt;
                    // Exponential drag: `exp(-k*dt)` compounds to the same
                    // total decay at any frame rate, unlike `1 - k*dt`.
                    let drag = (-0.6 * dt).exp();
                    for c in &mut p.vel {
                        *c *= drag;
                    }
                }
                TactileParticleKind::LavaSizzleSpark | TactileParticleKind::ImpactSpark => {
                    p.vel[1] -= 18.0 * dt;
                }
                TactileParticleKind::SteamPuff | TactileParticleKind::ColdBreathVapor => {
                    p.vel[1] += 0.6 * dt;
                    let drag = (-1.4 * dt).exp();
                    for c in &mut p.vel {
                        *c *= drag;
                    }
                    p.size += 0.10 * dt;
                }
                TactileParticleKind::HearthEmber => {
                    // Buoyant rise toward a kind-specific terminal speed,
                    // plus a deterministic sideways wobble.
                    let target = 0.7 + 0.5 * p.jitter;
                    p.vel[1] += (target - p.vel[1]) * (1.0 - (-3.0 * dt).exp());
                    p.vel[0] = 0.16 * (p.age_sec * 3.0 + p.phase).sin();
                    p.vel[2] = 0.16 * (p.age_sec * 2.4 + p.phase * 1.31).cos();
                }
                TactileParticleKind::LeafPetal => {
                    // Downward spiral: circular horizontal drift at a slowly
                    // shrinking radius, gentle constant descent.
                    let w = 2.2 + 1.3 * p.jitter;
                    let r = 0.30 * (1.0 - 0.4 * t);
                    p.vel[0] = r * w * (p.age_sec * w + p.phase).cos();
                    p.vel[1] = -0.55;
                    p.vel[2] = -r * w * (p.age_sec * w + p.phase).sin();
                }
                TactileParticleKind::FootstepPuff | TactileParticleKind::ArrowTrailMote => {
                    let drag = (-3.0 * dt).exp();
                    for c in &mut p.vel {
                        *c *= drag;
                    }
                }
                TactileParticleKind::WaterRippleRing { max_radius }
                | TactileParticleKind::BubblePopRing { max_radius }
                | TactileParticleKind::LandingDustRing { max_radius }
                | TactileParticleKind::DripLandingRing { max_radius }
                | TactileParticleKind::ShockRing { max_radius } => {
                    // Framerate-independent expansion: eased toward the
                    // kind's max radius purely from normalized age.
                    let ease = 1.0 - (1.0 - t) * (1.0 - t) * (1.0 - t);
                    p.size = *max_radius * ease;
                }
            }
            p.pos[0] += p.vel[0] * dt;
            p.pos[1] += p.vel[1] * dt;
            p.pos[2] += p.vel[2] * dt;
            for c in &mut p.pos {
                if !c.is_finite() {
                    *c = 0.0;
                }
            }
        }

        self.particles.retain(|p| {
            if p.expired() {
                if matches!(p.kind, TactileParticleKind::CeilingDrip) && valid_pos(&p.pos) {
                    landed_drips.push(p.pos);
                }
                false
            } else {
                true
            }
        });

        // Landing ripple cue where each ceiling drip finished its fall.
        for pos in landed_drips {
            if self.particles.len() >= self.particle_cap() {
                break;
            }
            let h = hash01(self.tick_index as i64, pos[0] as i64, SEED_DRIP);
            self.spawn_particle(
                TactileParticleKind::DripLandingRing {
                    max_radius: 0.16 + 0.08 * h,
                },
                pos,
                [0.0; 3],
                0.30,
                0.02,
                0.0,
                h,
                [0.70, 0.82, 0.95, 0.5],
            );
        }

        // Pending wet drips shed by the tracked body (water toggle only).
        let pending = self.wetness.take_pending_drips();
        if pending > 0 && self.effects_on() && self.config.water_and_lava_reactions {
            let base = self.wetness.position();
            for i in 0..pending.min(MAX_PENDING_DRIPS as usize) {
                let h = hash3(self.tick_index as i64, i as i64, SEED_WETDRIP);
                let pos = [
                    base[0] + (h[0] - 0.5) * 0.4,
                    base[1] + 0.3 + 0.9 * h[1],
                    base[2] + (h[2] - 0.5) * 0.4,
                ];
                self.spawn_particle(
                    TactileParticleKind::WetDrip,
                    pos,
                    [0.0, -0.1, 0.0],
                    0.9,
                    0.03,
                    h[0] * TAU,
                    h[1],
                    [0.50, 0.68, 0.95, 0.75],
                );
            }
        }
    }
}
