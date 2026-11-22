// SPDX-License-Identifier: LGPL-3.0-or-later OR GPL-3.0-or-later OR AGPL-3.0-or-later

//! Optional **Vanilla+ Nature & Sky** ambience pack for Megu.
//!
//! This module is **opt-in and default OFF**: [`VanillaPlusNatureConfig::default`]
//! has `enabled == false`, so the vanilla look and feel never change unless
//! the user explicitly turns the pack on. When enabled it layers four gentle,
//! low-cost upgrades on top of the classic blocky aesthetic:
//!
//! * **Layered voxel clouds** — a deterministic integer-grid cloud field that
//!   stays 100% square and pixel-consistent like vanilla clouds (never a
//!   blurry raymarched volume). Two to three stacked strata produce a wider
//!   base with a stepped, puffy upper cap, and hidden faces between adjacent
//!   cloud voxels are suppressed so quad counts stay tiny.
//! * **Subtle nature ambience** — a strictly bounded, rate-limited particle
//!   trickle (falling leaves incl. autumn/cherry tints, dusk fireflies, rain
//!   ripples on exposed water, waterfall mist) whose cap follows the active
//!   [`VfxBudgetTier`]. This is ambience, not particle spam.
//! * **Gentle weather-driven wind sway** — a small vertex offset for plants,
//!   crops, leaves and hanging lanterns. Grass and crop bases are pinned at
//!   `local_y == 0.0` so roots never detach; lanterns pivot from their top.
//!   [`MEGU_WIND_SWAY_WGSL_FN`] is a syntactically valid WGSL helper that
//!   terrain shaders can inline for the identical motion.
//! * **Minimal emissive mob eyes** — a lookup table that maps the vanilla
//!   mobs whose eyes or cores naturally glow to a subtle RGB tint and
//!   intensity. It is a glow mask hint only: zero model, rig or animation
//!   changes.
//!
//! Everything is pure `std`, deterministic, and reads no wall clock: all
//! animation derives from `world_time_ticks`, `dt_sec` and integer position
//! hashes. No Mojang or third-party geometry or texture data is referenced
//! anywhere in this file.

use std::collections::BTreeSet;
use std::f32::consts::{PI, TAU};

use crate::vfx_engine::VfxHardwareTier;

// ---------------------------------------------------------------------------
// Shared constants (fail-closed bounds)
// ---------------------------------------------------------------------------

/// Maximum absolute world coordinate accepted for cloud sampling. Matches the
/// classic far-lands guard used by [`crate::vfx_engine`].
const MAX_COORD: f32 = 30_000_000.0;

/// Cloud voxel footprint in blocks (vanilla clouds are chunky, not texel-fine).
pub const CLOUD_CELL_BLOCKS: f32 = 12.0;
/// Vertical thickness of one cloud stratum in blocks.
pub const CLOUD_LAYER_HEIGHT: f32 = 4.0;
/// Horizontal half-window of cloud cells generated around the camera.
pub const CLOUD_RADIUS_CELLS: i32 = 6;
/// Wind scroll speed in blocks per tick.
pub const CLOUD_SCROLL_BLOCKS_PER_TICK: f32 = 0.4;
/// Largest possible number of emitted cloud voxels (window x strata).
pub const MAX_CLOUD_CELLS: usize =
    ((2 * CLOUD_RADIUS_CELLS + 1) * (2 * CLOUD_RADIUS_CELLS + 1)) as usize * 3;

/// Exposed-face bitmask: face towards +X is visible.
pub const FACE_POS_X: u8 = 0b00_0001;
/// Exposed-face bitmask: face towards -X is visible.
pub const FACE_NEG_X: u8 = 0b00_0010;
/// Exposed-face bitmask: face towards +Y (top) is visible.
pub const FACE_POS_Y: u8 = 0b00_0100;
/// Exposed-face bitmask: face towards -Y (bottom) is visible.
pub const FACE_NEG_Y: u8 = 0b00_1000;
/// Exposed-face bitmask: face towards +Z is visible.
pub const FACE_POS_Z: u8 = 0b01_0000;
/// Exposed-face bitmask: face towards -Z is visible.
pub const FACE_NEG_Z: u8 = 0b10_0000;
/// All six faces exposed (isolated voxel).
pub const ALL_FACES: u8 = 0b11_1111;

const MIN_CLOUD_HEIGHT: f32 = 48.0;
const MAX_CLOUD_HEIGHT: f32 = 512.0;
const DEFAULT_CLOUD_HEIGHT: f32 = 192.0;

/// Per-site-list bound on spawn hints; extra sites are ignored.
pub const MAX_ENV_SITES: usize = 24;
/// Clamp for `dt_sec` so a hitch cannot explode the simulation.
const MAX_AMBIENCE_DT_SEC: f32 = 0.25;

const LOW_SPEC_AMBIENCE_CAP: usize = 32;
const BALANCED_AMBIENCE_CAP: usize = 96;
const HIGH_AMBIENCE_CAP: usize = 192;

const LEAF_PERIOD_MAX: u64 = 24;
const LEAF_PERIOD_MIN: u64 = 9;
const FIREFLY_PERIOD: u64 = 15;
const FIREFLIES_PER_EVENT: usize = 3;
const RIPPLE_PERIOD_RAIN: u64 = 5;
const RIPPLE_PERIOD_THUNDER: u64 = 3;
const MIST_PERIOD: u64 = 8;
const MIST_MIN_DROP: f32 = 3.0;

const WIND_SWAY_FREQUENCY: f32 = 1.4;
const WIND_TIME_WRAP_TICKS: u64 = 14_400;

// Deterministic hash seeds: one per feature so fields stay independent.
const SEED_CLOUD_A: u64 = 0x9e37_79b9_7f4a_7c15;
const SEED_CLOUD_B: u64 = 0xc2b2_ae3d_27d4_eb4f;
const SEED_LEAF: u64 = 0x1656_67b1_9e37_79f9;
const SEED_FIREFLY: u64 = 0x85eb_ca6b_f27d_4a3d;
const SEED_RIPPLE: u64 = 0x27d4_eb2f_1656_67c5;
const SEED_MIST: u64 = 0x94d0_49bb_1331_11eb;
const SEED_SWAY: u64 = 0x632d_e59f_0a5a_2b71;

// Density thresholds per stratum: base is widest, each cap a smaller subset.
const DENSITY_BASE: f32 = 0.60;
const DENSITY_CAP_1: f32 = 0.72;
const DENSITY_CAP_2: f32 = 0.84;

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

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        lerp(a[0], b[0], t),
        lerp(a[1], b[1], t),
        lerp(a[2], b[2], t),
    ]
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = clamped((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Deterministic integer-coordinate hash -> `[0, 1]`. No RNG, no clock,
/// stable across rebuilds and platforms.
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

/// Smooth value noise over the integer grid: hash corners + smoothstepped
/// bilinear interpolation. Gives cohesive blobs, not per-cell static.
fn value_noise_2d(x: f64, z: f64, seed: u64) -> f32 {
    let xi = x.floor() as i64;
    let zi = z.floor() as i64;
    let xf = (x - xi as f64) as f32;
    let zf = (z - zi as f64) as f32;
    let u = xf * xf * (3.0 - 2.0 * xf);
    let v = zf * zf * (3.0 - 2.0 * zf);
    let a = hash01(xi, zi, seed);
    let b = hash01(xi + 1, zi, seed);
    let c = hash01(xi, zi + 1, seed);
    let d = hash01(xi + 1, zi + 1, seed);
    lerp(lerp(a, b, u), lerp(c, d, u), v)
}

/// Two-octave cloud coverage field in `[0, 1]` keyed purely by integer cell.
fn cloud_density(cx: i64, cz: i64) -> f32 {
    let n1 = value_noise_2d(cx as f64 / 9.0, cz as f64 / 9.0, SEED_CLOUD_A);
    let n2 = value_noise_2d(cx as f64 / 3.0, cz as f64 / 3.0, SEED_CLOUD_B);
    0.7 * n1 + 0.3 * n2
}

/// Per-block sway phase offset so neighbouring plants never move in lockstep.
fn block_phase(block: [i32; 3]) -> f32 {
    hash01(
        (block[0] as i64).wrapping_mul(31) + block[1] as i64,
        (block[2] as i64).wrapping_mul(17) + block[1] as i64,
        SEED_SWAY,
    ) * TAU
}

/// Strips a `namespace:` prefix and any trailing state suffix from an id.
fn entity_path(entity_id: &str) -> &str {
    let path = entity_id.rsplit(':').next().unwrap_or(entity_id);
    path.trim()
}

// ---------------------------------------------------------------------------
// Master opt-in configuration
// ---------------------------------------------------------------------------

/// Per-user configuration for the Vanilla+ Nature & Sky pack. `Default` is
/// fully vanilla: `enabled == false`.
#[derive(Clone, Debug, PartialEq)]
pub struct VanillaPlusNatureConfig {
    /// Master opt-in switch. **`false` by default** — the pack never changes
    /// the vanilla look unless explicitly enabled.
    pub enabled: bool,
    /// Draw layered voxel clouds on top of (or instead of) the flat vanilla
    /// cloud plane.
    pub layered_voxel_clouds: bool,
    /// Number of vertical cloud strata, clamped to `1..=3` when read via
    /// [`Self::effective_cloud_layers`].
    pub cloud_layers: u8,
    /// Cloud base altitude in blocks. Default `192.0` (vanilla height),
    /// clamped to `48.0..=512.0` via [`Self::effective_cloud_height`].
    pub cloud_height: f32,
    /// Falling leaves, dusk fireflies, rain ripples and waterfall mist.
    pub nature_ambience: bool,
    /// Weather-linked sway for grass, crops, leaves and hanging lanterns.
    pub gentle_wind_sway: bool,
    /// Subtle glow mask on vanilla mobs with naturally glowing features.
    /// Tint and intensity only — zero geometry or animation changes.
    pub emissive_mob_eyes: bool,
}

impl Default for VanillaPlusNatureConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            layered_voxel_clouds: true,
            cloud_layers: 2,
            cloud_height: DEFAULT_CLOUD_HEIGHT,
            nature_ambience: true,
            gentle_wind_sway: true,
            emissive_mob_eyes: true,
        }
    }
}

impl VanillaPlusNatureConfig {
    /// `cloud_layers` clamped to `1..=3`.
    pub fn effective_cloud_layers(&self) -> u8 {
        self.cloud_layers.clamp(1, 3)
    }

    /// `cloud_height` clamped to `48.0..=512.0` (non-finite -> 192).
    pub fn effective_cloud_height(&self) -> f32 {
        finite_or(self.cloud_height, DEFAULT_CLOUD_HEIGHT).clamp(MIN_CLOUD_HEIGHT, MAX_CLOUD_HEIGHT)
    }
}

// ---------------------------------------------------------------------------
// Budget tier & weather primitives
// ---------------------------------------------------------------------------

/// Ambience budget tier, mirroring [`VfxHardwareTier`] semantics at a far
/// smaller (deliberately subtle) scale.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VfxBudgetTier {
    /// Potato-grade: hard cap 32 live ambience particles, 2 spawns/tick.
    LowSpec,
    /// Default: hard cap 96 live ambience particles, 6 spawns/tick.
    #[default]
    Balanced,
    /// Strong GPU: hard cap 192 live ambience particles, 12 spawns/tick.
    High,
}

impl VfxBudgetTier {
    /// Hard cap on simultaneously live ambience particles.
    pub const fn max_particles(self) -> usize {
        match self {
            Self::LowSpec => LOW_SPEC_AMBIENCE_CAP,
            Self::Balanced => BALANCED_AMBIENCE_CAP,
            Self::High => HIGH_AMBIENCE_CAP,
        }
    }

    /// Rate limit on new ambience particles per tick.
    pub const fn max_spawns_per_tick(self) -> usize {
        match self {
            Self::LowSpec => 2,
            Self::Balanced => 6,
            Self::High => 12,
        }
    }
}

impl From<VfxHardwareTier> for VfxBudgetTier {
    fn from(tier: VfxHardwareTier) -> Self {
        match tier {
            VfxHardwareTier::LowSpec => Self::LowSpec,
            VfxHardwareTier::Balanced => Self::Balanced,
            VfxHardwareTier::High => Self::High,
        }
    }
}

/// Coarse precipitation state driving ambience and wind strength.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WeatherKind {
    /// No precipitation: gentlest breeze.
    #[default]
    Clear,
    /// Rainfall: moderate wind, ripples on exposed water.
    Rain,
    /// Thunderstorm: strongest wind, faster ripples.
    Thunder,
}

impl WeatherKind {
    /// `true` when rain or a thunderstorm is active.
    pub const fn is_precipitating(self) -> bool {
        !matches!(self, Self::Clear)
    }

    /// Wind amplitude scale: clear breeze 0.15, rain 0.45, thunder 0.85.
    pub const fn wind_strength(self) -> f32 {
        match self {
            Self::Clear => 0.15,
            Self::Rain => 0.45,
            Self::Thunder => 0.85,
        }
    }
}

// ---------------------------------------------------------------------------
// Layered voxel clouds
// ---------------------------------------------------------------------------

/// One visible cloud voxel on the integer grid. Faces shared with a
/// neighbouring cloud voxel are suppressed via [`Self::exposed_faces`], so
/// the emitted quad count stays tiny.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoxelCloudCell {
    /// World-space minimum corner of the voxel.
    pub min: [f32; 3],
    /// Voxel extents: `(CLOUD_CELL_BLOCKS, CLOUD_LAYER_HEIGHT, CLOUD_CELL_BLOCKS)`.
    pub size: [f32; 3],
    /// Stratum index: `0` is the wide base, `1..` are stepped puffy caps.
    pub layer: u8,
    /// Bitmask of `FACE_*` bits for faces not shared with a neighbour.
    pub exposed_faces: u8,
}

impl VoxelCloudCell {
    /// Number of quads a mesher has to emit for this voxel (0..=6).
    pub fn quad_count(&self) -> u32 {
        self.exposed_faces.count_ones()
    }
}

/// Time-of-day and weather lighting for the cloud layer. All fields are
/// already gamma-ready linear-ish tints in `0..=1`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CloudLighting {
    /// Up-facing quad tint: crisp white by day, deep slate by night, warm
    /// peach/amber at the horizon during sunrise and sunset.
    pub top_rgb: [f32; 3],
    /// Side-facing quad tint.
    pub side_rgb: [f32; 3],
    /// Down-facing quad tint: soft cool underside by day, heavily darkened
    /// under rain or a thunderstorm.
    pub bottom_rgb: [f32; 3],
    /// Warm horizon-rim strength in `0..=1` (peaks at `sun_elevation ~= 0`).
    pub rim_warmth: f32,
}

/// Computes the cloud-layer tint for a sun elevation in `-1..=1` (negative:
/// below the horizon, i.e. night) and precipitation strengths in `0..=1`.
pub fn compute_cloud_lighting(
    sun_elevation: f32,
    rain_strength: f32,
    thunder_strength: f32,
) -> CloudLighting {
    let e = clamped(sun_elevation, -1.0, 1.0);
    let rain = clamped(rain_strength, 0.0, 1.0);
    let thunder = clamped(thunder_strength, 0.0, 1.0);

    // 0 = deep night, 1 = full day. A little margin keeps late dusk moody.
    let day = smoothstep(-0.10, 0.22, e);
    // Warm rim only while the sun is near AND above the horizon: a midnight
    // sun_elevation of -0.4 stays cool, not peach.
    let dusk = 1.0 - smoothstep(0.0, 0.4, e.abs());
    let above = smoothstep(-0.16, -0.02, e);
    let rim = dusk * above;

    let night_top = [0.10, 0.12, 0.21];
    let night_side = [0.07, 0.09, 0.17];
    let night_bottom = [0.05, 0.07, 0.13];
    let day_top = [0.99, 1.0, 1.0];
    let day_side = [0.84, 0.88, 0.95];
    let day_bottom = [0.62, 0.68, 0.80];
    let amber = [1.0, 0.62, 0.34];

    let mut top = lerp3(night_top, day_top, day);
    let mut side = lerp3(night_side, day_side, day);
    let bottom = lerp3(night_bottom, day_bottom, day);

    top = lerp3(top, amber, rim * 0.55);
    side = lerp3(side, amber, rim * 0.30);

    // Storm underside darkening; the top greys only slightly.
    let storm = (rain * 0.55 + thunder).clamp(0.0, 1.0);
    let under_mul = 1.0 - 0.65 * storm;
    let top_mul = 1.0 - 0.25 * storm;
    let side_mul = 1.0 - 0.40 * storm;
    for i in 0..3 {
        top[i] *= top_mul;
        side[i] *= side_mul;
    }
    let bottom = [
        bottom[0] * under_mul,
        bottom[1] * under_mul,
        bottom[2] * under_mul,
    ];

    CloudLighting {
        top_rgb: top,
        side_rgb: side,
        bottom_rgb: bottom,
        rim_warmth: rim,
    }
}

/// Generates the layered voxel cloud field around `(camera_x, camera_z)`.
///
/// Deterministic for identical arguments: the field is an integer-grid value
/// noise keyed by cell coordinates, scrolled by `world_time_ticks` at
/// [`CLOUD_SCROLL_BLOCKS_PER_TICK`]. Cells drift smoothly within a cell
/// period and appear/vanish only at whole-cell boundaries — exactly the
/// chunky vanilla feel. Hidden faces between adjacent cloud voxels (and
/// between stacked strata) are suppressed in [`VoxelCloudCell::exposed_faces`].
///
/// Returns an empty `Vec` unless `config.enabled && config.layered_voxel_clouds`.
/// Output is bounded by [`MAX_CLOUD_CELLS`] and all positions are finite.
pub fn generate_layered_voxel_clouds(
    config: &VanillaPlusNatureConfig,
    camera_x: f32,
    camera_z: f32,
    world_time_ticks: u64,
) -> Vec<VoxelCloudCell> {
    if !config.enabled || !config.layered_voxel_clouds {
        return Vec::new();
    }
    let layers = config.effective_cloud_layers();
    let height = config.effective_cloud_height();

    let cam_x = finite_or(camera_x, 0.0).clamp(-MAX_COORD, MAX_COORD) as f64;
    let cam_z = finite_or(camera_z, 0.0).clamp(-MAX_COORD, MAX_COORD) as f64;
    // u64::MAX ticks * 0.4 stays far below i64/f64 range limits.
    let scroll = world_time_ticks as f64 * CLOUD_SCROLL_BLOCKS_PER_TICK as f64;

    let cell = CLOUD_CELL_BLOCKS as f64;
    let cx0 = ((cam_x + scroll) / cell).floor() as i64;
    let cz0 = (cam_z / cell).floor() as i64;

    // Occupancy set for hidden-face suppression across all strata.
    let mut occupied: BTreeSet<(i64, u8, i64)> = BTreeSet::new();
    let mut cells: Vec<VoxelCloudCell> = Vec::new();
    for dz in -CLOUD_RADIUS_CELLS..=CLOUD_RADIUS_CELLS {
        for dx in -CLOUD_RADIUS_CELLS..=CLOUD_RADIUS_CELLS {
            let cx = cx0 + i64::from(dx);
            let cz = cz0 + i64::from(dz);
            let density = cloud_density(cx, cz);
            for layer in 0..layers {
                let threshold = match layer {
                    0 => DENSITY_BASE,
                    1 => DENSITY_CAP_1,
                    _ => DENSITY_CAP_2,
                };
                if density > threshold {
                    occupied.insert((cx, layer, cz));
                }
            }
        }
    }

    for dz in -CLOUD_RADIUS_CELLS..=CLOUD_RADIUS_CELLS {
        for dx in -CLOUD_RADIUS_CELLS..=CLOUD_RADIUS_CELLS {
            let cx = cx0 + i64::from(dx);
            let cz = cz0 + i64::from(dz);
            for layer in 0..layers {
                if !occupied.contains(&(cx, layer, cz)) {
                    continue;
                }
                let mut mask = ALL_FACES;
                if occupied.contains(&(cx + 1, layer, cz)) {
                    mask &= !FACE_POS_X;
                }
                if occupied.contains(&(cx - 1, layer, cz)) {
                    mask &= !FACE_NEG_X;
                }
                if occupied.contains(&(cx, layer + 1, cz)) {
                    mask &= !FACE_POS_Y;
                }
                if layer > 0 && occupied.contains(&(cx, layer - 1, cz)) {
                    mask &= !FACE_NEG_Y;
                }
                if occupied.contains(&(cx, layer, cz + 1)) {
                    mask &= !FACE_POS_Z;
                }
                if occupied.contains(&(cx, layer, cz - 1)) {
                    mask &= !FACE_NEG_Z;
                }
                if mask == 0 {
                    continue;
                }
                cells.push(VoxelCloudCell {
                    min: [
                        (cx as f64 * cell - scroll) as f32,
                        height + f32::from(layer) * CLOUD_LAYER_HEIGHT,
                        (cz as f64 * cell) as f32,
                    ],
                    size: [CLOUD_CELL_BLOCKS, CLOUD_LAYER_HEIGHT, CLOUD_CELL_BLOCKS],
                    layer,
                    exposed_faces: mask,
                });
            }
        }
    }
    cells
}

// ---------------------------------------------------------------------------
// Subtle nature ambience particles
// ---------------------------------------------------------------------------

/// One bounded ambience particle. `kind` carries the per-kind colour/phase
/// data; `pos`/`vel`/`age_sec`/`ttl_sec` are the shared kinematics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NatureParticle {
    pub kind: NatureParticleKind,
    pub pos: [f32; 3],
    pub vel: [f32; 3],
    pub age_sec: f32,
    pub ttl_sec: f32,
}

impl NatureParticle {
    /// `true` once the particle should be dropped.
    pub fn expired(&self) -> bool {
        self.age_sec >= self.ttl_sec
            || matches!(self.kind, NatureParticleKind::RainRipple { radius, max_radius } if radius >= max_radius)
    }
}

/// The four ambience particle families. Each stays deliberately quiet:
/// small counts, soft colours, no screen-space noise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NatureParticleKind {
    /// A leaf drifting down from an exposed canopy block. `rgb` carries the
    /// tint: green in spring/summer, amber-red with `autumn_factor`, pink in
    /// cherry groves. `flutter_phase` desynchronises the side-to-side sway.
    FallingLeaf { rgb: [f32; 3], flutter_phase: f32 },
    /// A soft-pulsing ember of light in warm/temperate biomes at night.
    Firefly {
        glow_rgb: [f32; 3],
        pulse_phase: f32,
    },
    /// An expanding ring on a rain-exposed water surface.
    RainRipple { radius: f32, max_radius: f32 },
    /// A soft white puff at the base of a vertical water drop.
    WaterfallMist { alpha: f32 },
}

/// World-sampled spawn hints for one ambience tick. Callers fill the site
/// lists from whatever chunk data they already have; each list is read up to
/// [`MAX_ENV_SITES`] entries and non-finite positions are skipped.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NatureAmbienceEnv {
    /// Current precipitation state.
    pub weather: WeatherKind,
    /// Dusk/night flag gating fireflies.
    pub is_night: bool,
    /// `true` in warm/temperate biomes (swamp, mangrove, plains, meadow).
    /// Freezing and desert biomes must pass `false`.
    pub firefly_biome: bool,
    /// `true` under cherry-blossom canopy: leaves fall as pink petals.
    pub cherry_blossom: bool,
    /// Seasonal autumn amount in `0..=1` (feeds the Season engine through).
    pub autumn_factor: f32,
    /// Camera/player anchor for free-roaming spawns (fireflies).
    pub origin: [f32; 3],
    /// Exposed leaf/canopy block centres that can shed a leaf.
    pub leaf_blocks: Vec<[f32; 3]>,
    /// Rain-exposed water surface points (ripples only while precipitating).
    pub rain_exposed_water: Vec<[f32; 3]>,
    /// `(base position, vertical drop height)` of water columns; mist only
    /// forms for drops of at least 3 blocks.
    pub waterfall_bases: Vec<([f32; 3], f32)>,
}

/// Retained ambience state: a strictly capped particle `Vec` plus the
/// deterministic tick counter used for rate-limited spawning.
#[derive(Clone, Debug)]
pub struct NatureAmbienceState {
    tier: VfxBudgetTier,
    tick_index: u64,
    particles: Vec<NatureParticle>,
}

impl Default for NatureAmbienceState {
    fn default() -> Self {
        Self::new(VfxBudgetTier::Balanced)
    }
}

impl NatureAmbienceState {
    pub fn new(tier: VfxBudgetTier) -> Self {
        Self {
            tier,
            tick_index: 0,
            particles: Vec::new(),
        }
    }

    pub const fn tier(&self) -> VfxBudgetTier {
        self.tier
    }

    /// Re-tiers the budget; live particles beyond the new cap are trimmed
    /// (oldest first) so the invariant holds immediately.
    pub fn set_tier(&mut self, tier: VfxBudgetTier) {
        self.tier = tier;
        let cap = tier.max_particles();
        if self.particles.len() > cap {
            let excess = self.particles.len() - cap;
            self.particles.drain(..excess);
        }
    }

    pub fn particles(&self) -> &[NatureParticle] {
        &self.particles
    }

    pub fn len(&self) -> usize {
        self.particles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.particles.is_empty()
    }

    fn try_spawn(&mut self, quota: &mut usize, particle: NatureParticle) {
        if *quota == 0 || self.particles.len() >= self.tier.max_particles() {
            return;
        }
        *quota -= 1;
        self.particles.push(particle);
    }
}

fn valid_site(p: &[f32; 3]) -> bool {
    p.iter().all(|c| c.is_finite() && c.abs() <= MAX_COORD)
}

/// Advances the ambience simulation by one tick.
///
/// Ageing and motion are deterministic functions of `dt_sec` (clamped to
/// `0..=0.25`) and each particle's stored phase; spawning is a deterministic
/// function of `tick_index` and site index — no RNG, no clock. The particle
/// count can never exceed `state.tier().max_particles()` and no more than
/// `max_spawns_per_tick()` particles appear in one call. When the pack or
/// the ambience toggle is off, existing particles still age out naturally
/// but nothing new spawns.
pub fn tick_nature_ambience(
    state: &mut NatureAmbienceState,
    config: &VanillaPlusNatureConfig,
    env: &NatureAmbienceEnv,
    dt_sec: f32,
) {
    let dt = if dt_sec.is_finite() {
        dt_sec.clamp(0.0, MAX_AMBIENCE_DT_SEC)
    } else {
        0.0
    };
    state.tick_index = state.tick_index.wrapping_add(1);
    let tick = state.tick_index;

    for p in state.particles.iter_mut() {
        p.age_sec += dt;
        match &mut p.kind {
            NatureParticleKind::FallingLeaf { flutter_phase, .. } => {
                let ph = *flutter_phase;
                p.vel = [
                    0.28 * (p.age_sec * 2.1 + ph).sin(),
                    -0.5,
                    0.28 * (p.age_sec * 1.7 + ph * 1.31).cos(),
                ];
            }
            NatureParticleKind::Firefly { pulse_phase, .. } => {
                let ph = *pulse_phase;
                p.vel = [
                    0.22 * (p.age_sec * 0.9 + ph).sin(),
                    0.16 * (p.age_sec * 1.3 + ph * 2.0).sin(),
                    0.22 * (p.age_sec * 0.8 + ph).cos(),
                ];
            }
            NatureParticleKind::RainRipple { radius, .. } => {
                *radius += 1.8 * dt;
            }
            NatureParticleKind::WaterfallMist { alpha } => {
                p.vel = [0.05 * (p.age_sec * 1.4).sin(), 0.35, 0.05];
                let t = clamped(p.age_sec / p.ttl_sec, 0.0, 1.0);
                *alpha = 0.35 * (PI * t).sin().max(0.0);
            }
        }
        p.pos[0] += p.vel[0] * dt;
        p.pos[1] += p.vel[1] * dt;
        p.pos[2] += p.vel[2] * dt;
    }
    state.particles.retain(|p| !p.expired());

    if !config.enabled || !config.nature_ambience {
        return;
    }
    let mut quota = state.tier.max_spawns_per_tick();

    // Falling leaves (incl. autumn amber and cherry petals).
    let autumn = clamped(env.autumn_factor, 0.0, 1.0);
    if !env.leaf_blocks.is_empty() {
        let period = (LEAF_PERIOD_MAX as f32 - (LEAF_PERIOD_MAX - LEAF_PERIOD_MIN) as f32 * autumn)
            .round() as u64;
        for (i, site) in env.leaf_blocks.iter().take(MAX_ENV_SITES).enumerate() {
            if !valid_site(site) || !(tick + i as u64 * 7).is_multiple_of(period) {
                continue;
            }
            let h1 = hash01(i as i64, tick as i64, SEED_LEAF);
            let h2 = hash01(tick as i64, i as i64, SEED_LEAF ^ 0x5a5a);
            let pos = [
                site[0] + (h1 - 0.5) * 0.8,
                site[1],
                site[2] + (h2 - 0.5) * 0.8,
            ];
            let rgb = if env.cherry_blossom {
                [0.98, 0.68 + 0.10 * h2, 0.80]
            } else {
                let t = autumn * (0.6 + 0.4 * h1);
                lerp3([0.30, 0.52, 0.20], [0.90, 0.46, 0.10], t)
            };
            state.try_spawn(
                &mut quota,
                NatureParticle {
                    kind: NatureParticleKind::FallingLeaf {
                        rgb,
                        flutter_phase: h1 * TAU,
                    },
                    pos,
                    vel: [0.0, -0.5, 0.0],
                    age_sec: 0.0,
                    ttl_sec: 5.0 + 4.0 * h2,
                },
            );
        }
    }

    // Fireflies: dusk/night only, warm/temperate biomes only.
    if env.is_night && env.firefly_biome && tick.is_multiple_of(FIREFLY_PERIOD) {
        let origin = [
            finite_or(env.origin[0], 0.0).clamp(-MAX_COORD, MAX_COORD),
            finite_or(env.origin[1], 64.0).clamp(-64.0, 384.0),
            finite_or(env.origin[2], 0.0).clamp(-MAX_COORD, MAX_COORD),
        ];
        for k in 0..FIREFLIES_PER_EVENT {
            let h1 = hash01(tick as i64, k as i64, SEED_FIREFLY);
            let h2 = hash01(k as i64, tick as i64, SEED_FIREFLY ^ 0x33cc);
            let angle = h1 * TAU;
            let radius = 5.0 + 9.0 * h2;
            let pos = [
                origin[0] + angle.cos() * radius,
                origin[1] + 0.5 + 2.5 * h1,
                origin[2] + angle.sin() * radius,
            ];
            state.try_spawn(
                &mut quota,
                NatureParticle {
                    kind: NatureParticleKind::Firefly {
                        glow_rgb: [0.80 + 0.20 * h1, 1.0, 0.30 + 0.15 * h2],
                        pulse_phase: h2 * TAU,
                    },
                    pos,
                    vel: [0.0; 3],
                    age_sec: 0.0,
                    ttl_sec: 8.0 + 6.0 * h2,
                },
            );
        }
    }

    // Rain ripples: precipitation only, exposed water surfaces only.
    if env.weather.is_precipitating() {
        let period = match env.weather {
            WeatherKind::Thunder => RIPPLE_PERIOD_THUNDER,
            _ => RIPPLE_PERIOD_RAIN,
        };
        for (i, site) in env
            .rain_exposed_water
            .iter()
            .take(MAX_ENV_SITES)
            .enumerate()
        {
            if !valid_site(site) || !(tick + i as u64 * 3).is_multiple_of(period) {
                continue;
            }
            let h = hash01(i as i64, tick as i64, SEED_RIPPLE);
            let max_radius = 0.7 + 0.5 * h;
            state.try_spawn(
                &mut quota,
                NatureParticle {
                    kind: NatureParticleKind::RainRipple {
                        radius: 0.08,
                        max_radius,
                    },
                    pos: *site,
                    vel: [0.0; 3],
                    age_sec: 0.0,
                    ttl_sec: max_radius / 1.8 + 0.5,
                },
            );
        }
    }

    // Waterfall mist: only at the base of real drops (>= 3 blocks).
    for (i, (base, drop_height)) in env.waterfall_bases.iter().take(MAX_ENV_SITES).enumerate() {
        if !valid_site(base)
            || !drop_height.is_finite()
            || *drop_height < MIST_MIN_DROP
            || !(tick + i as u64 * 5).is_multiple_of(MIST_PERIOD)
        {
            continue;
        }
        let h = hash01(i as i64, tick as i64, SEED_MIST);
        state.try_spawn(
            &mut quota,
            NatureParticle {
                kind: NatureParticleKind::WaterfallMist { alpha: 0.0 },
                pos: [
                    base[0] + (h - 0.5) * 0.6,
                    base[1] + 0.2,
                    base[2] + (0.5 - h) * 0.6,
                ],
                vel: [0.0, 0.35, 0.0],
                age_sec: 0.0,
                ttl_sec: 2.5 + 1.5 * h,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Gentle weather-driven wind sway
// ---------------------------------------------------------------------------

/// Plant category choosing how the sway amplitude distributes over local Y.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindSwayCategory {
    /// Grass, ferns, flowers, saplings: base pinned, tip sways.
    ShortGrass,
    /// Crops (wheat, carrots, ...): same pinned-base profile as grass.
    Crop,
    /// Leaf blocks: a tiny uniform rustle, no pivot.
    Leaves,
    /// Lanterns and chains hanging from a top pivot point.
    HangingLantern,
}

/// Time-decoupled wind parameters produced once per frame/tick and shared by
/// every swayed vertex.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindSwayParams {
    /// Scaled amplitude in blocks. `0.0` when the pack or the sway toggle is
    /// off — a zero-strength parameter set always yields zero offsets.
    pub strength: f32,
    /// Animation time in seconds derived from `world_time_ticks`.
    pub time_sec: f32,
    /// Angular frequency in radians per second.
    pub frequency: f32,
}

/// Builds sway parameters for the current weather and tick. `strength` is
/// `0.0` unless `config.enabled && config.gentle_wind_sway`, so callers can
/// feed the result straight into [`compute_wind_sway`] without branching.
pub fn wind_sway_params(
    config: &VanillaPlusNatureConfig,
    weather: WeatherKind,
    world_time_ticks: u64,
) -> WindSwayParams {
    let strength = if config.enabled && config.gentle_wind_sway {
        weather.wind_strength()
    } else {
        0.0
    };
    WindSwayParams {
        strength,
        time_sec: (world_time_ticks % WIND_TIME_WRAP_TICKS) as f32 * 0.05,
        frequency: WIND_SWAY_FREQUENCY,
    }
}

/// Per-vertex sway offset in blocks.
///
/// * `ShortGrass`/`Crop`: amplitude scales with `local_y.clamp(0,1)`, so the
///   base at `local_y == 0.0` is always pinned at exactly `[0,0,0]` — roots
///   never detach.
/// * `Leaves`: a small uniform 3-axis rustle.
/// * `HangingLantern`: amplitude scales with `1 - local_y` clamped to `0..=1`,
///   so the top pivot at `local_y == 1.0` is pinned and the body swings below.
///
/// Output is always finite; non-finite `local` components fall back to the
/// pinned position (0.0 for plants, 1.0 for lanterns).
pub fn compute_wind_sway(
    params: &WindSwayParams,
    category: WindSwayCategory,
    block: [i32; 3],
    local: [f32; 3],
) -> [f32; 3] {
    let phase = params.time_sec * params.frequency + block_phase(block);
    let s0 = phase.sin();
    let s1 = (phase * 0.63 + 1.3).sin();
    match category {
        WindSwayCategory::ShortGrass | WindSwayCategory::Crop => {
            let attach = finite_or(local[1], 0.0).clamp(0.0, 1.0);
            let amp = params.strength * 0.12 * attach;
            [amp * s0, 0.0, amp * s1 * 0.6]
        }
        WindSwayCategory::Leaves => {
            let lx = finite_or(local[0], 0.0);
            let ly = finite_or(local[1], 0.0);
            let lz = finite_or(local[2], 0.0);
            let amp = params.strength * 0.05;
            [
                amp * (phase * 1.7 + lx * 3.1).sin(),
                amp * 0.4 * (phase * 1.9 + ly * 2.3).cos(),
                amp * (phase * 1.6 + lz * 2.7).sin(),
            ]
        }
        WindSwayCategory::HangingLantern => {
            let attach = (1.0 - finite_or(local[1], 1.0)).clamp(0.0, 1.0);
            let amp = params.strength * 0.10 * attach;
            [amp * s0, 0.0, amp * s1]
        }
    }
}

/// Syntactically valid WGSL helper computing the same sway on the GPU.
/// `kind` is the [`WindSwayCategory`] discriminant (`0` grass, `1` crop,
/// `2` leaves, `3` hanging lantern); `block_phase` is the per-block hash phase
/// computed identically to [`block_phase`].
pub const MEGU_WIND_SWAY_WGSL_FN: &str = r#"
fn megu_wind_sway(local_pos: vec3<f32>, block_phase: f32, time_sec: f32,
                  strength: f32, kind: u32) -> vec3<f32> {
    let phase = time_sec * 1.4 + block_phase;
    let s0 = sin(phase);
    let s1 = sin(phase * 0.63 + 1.3);
    var attach = clamp(local_pos.y, 0.0, 1.0);
    var amp = strength * 0.12;
    if (kind == 2u) {
        attach = 1.0;
        amp = strength * 0.05;
    } else if (kind == 3u) {
        attach = clamp(1.0 - local_pos.y, 0.0, 1.0);
        amp = strength * 0.10;
    }
    if (kind == 2u) {
        return vec3<f32>(amp * sin(phase * 1.7 + local_pos.x * 3.1),
                         amp * 0.4 * cos(phase * 1.9 + local_pos.y * 2.3),
                         amp * sin(phase * 1.6 + local_pos.z * 2.7));
    }
    return vec3<f32>(amp * attach * s0, 0.0, amp * attach * s1 * 0.6);
}
"#;

// ---------------------------------------------------------------------------
// Minimal emissive mob eyes
// ---------------------------------------------------------------------------

/// Which naturally-glowing vanilla feature the hint applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MobEmissiveFeature {
    /// Eye glow mask only (enderman, spiders, phantom, drowned).
    Eyes,
    /// Flame rods / burning core (blaze, magma cube).
    FlameOrCore,
    /// Full soft body glow (glow squid, allay).
    BodyGlow,
}

/// A subtle emissive tint for one mob feature: colour plus `0..=1`
/// intensity. Purely a glow-mask hint — no geometry, rig or UV change.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MobEmissiveHint {
    pub feature: MobEmissiveFeature,
    pub rgb: [f32; 3],
    pub intensity: f32,
}

/// Maps vanilla mobs with naturally glowing features to a subtle emissive
/// hint. `entity_id` may be namespaced (`"minecraft:enderman"`); only the
/// path component is matched. Returns `None` for everything else — the table
/// is intentionally minimal.
pub fn emissive_mob_feature(entity_id: &str) -> Option<MobEmissiveHint> {
    let path = entity_path(entity_id);
    let hint = |feature, rgb, intensity| {
        Some(MobEmissiveHint {
            feature,
            rgb,
            intensity,
        })
    };
    match path {
        "enderman" => hint(MobEmissiveFeature::Eyes, [0.72, 0.28, 0.88], 0.85),
        "spider" | "cave_spider" => hint(MobEmissiveFeature::Eyes, [0.90, 0.10, 0.08], 0.70),
        "phantom" => hint(MobEmissiveFeature::Eyes, [0.62, 0.88, 0.72], 0.60),
        "drowned" => hint(MobEmissiveFeature::Eyes, [0.20, 0.80, 0.80], 0.55),
        "blaze" => hint(MobEmissiveFeature::FlameOrCore, [1.00, 0.60, 0.10], 0.90),
        "magma_cube" => hint(MobEmissiveFeature::FlameOrCore, [1.00, 0.35, 0.05], 0.80),
        "warden" => hint(MobEmissiveFeature::FlameOrCore, [0.20, 0.90, 0.85], 0.80),
        "glow_squid" => hint(MobEmissiveFeature::BodyGlow, [0.30, 0.90, 0.85], 0.70),
        "allay" => hint(MobEmissiveFeature::BodyGlow, [0.55, 0.80, 1.00], 0.50),
        _ => None,
    }
}

/// Config-aware wrapper around [`emissive_mob_feature`]: returns `None`
/// unless `config.enabled && config.emissive_mob_eyes`.
pub fn emissive_mob_hint(
    config: &VanillaPlusNatureConfig,
    entity_id: &str,
) -> Option<MobEmissiveHint> {
    if config.enabled && config.emissive_mob_eyes {
        emissive_mob_feature(entity_id)
    } else {
        None
    }
}
